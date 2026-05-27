use std::borrow::Borrow;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadQuery};

fn make_event(sequence: u64, event_type: EventType, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

/// append した Event を load(by_stream) で取得し、payload・sequence が一致する
#[tokio::test]
async fn append_and_load_by_stream_matches_payload_and_sequence() {
    // Given: an empty store and one event to append
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");
    let event = make_event(1, et, b"hello world");

    // When: the event is appended and then loaded by stream key
    store
        .append(agg.borrow(), vec![event])
        .await
        .expect("append should succeed");
    let stream = store
        .load(LoadQuery::by_stream(&agg))
        .await
        .expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");

    // Then: the loaded event matches what was appended
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stream_key, agg.as_str());
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[0].event_type, et);
    assert_eq!(events[0].payload, Bytes::from_static(b"hello world"));
}

/// 複数 Aggregate に append し、from_global で global_sequence 昇順に取得できる
#[tokio::test]
async fn load_from_global_returns_events_in_global_sequence_order() {
    // Given: two aggregates, three events appended across them
    let store = InMemoryEventStore::default();
    let agg1 = AggregateId::new("agg-1");
    let agg2 = AggregateId::new("agg-2");
    let et = EventType::from_str("TestEvent");

    // When: events are appended to different aggregates, then loaded from global sequence 1
    store
        .append(agg1.borrow(), vec![make_event(1, et, b"a1e1")])
        .await
        .expect("append agg1 e1 should succeed");
    store
        .append(agg2.borrow(), vec![make_event(1, et, b"a2e1")])
        .await
        .expect("append agg2 e1 should succeed");
    store
        .append(agg1.borrow(), vec![make_event(2, et, b"a1e2")])
        .await
        .expect("append agg1 e2 should succeed");

    let stream = store
        .load(LoadQuery::from_global(1))
        .await
        .expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");

    // Then: all three events are returned in ascending global_sequence order
    assert_eq!(events.len(), 3);
    let global_seqs: Vec<u64> = events.iter().map(|e| e.global_sequence).collect();
    let mut sorted = global_seqs.clone();
    sorted.sort_unstable();
    assert_eq!(
        global_seqs, sorted,
        "events must be returned in global_sequence order"
    );
}

/// by_event_type フィルタで該当する EventType のみ返る
#[tokio::test]
async fn load_by_event_type_returns_only_matching_events() {
    // Given: one aggregate with three events of two different types
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let type_a = EventType::from_str("TypeA");
    let type_b = EventType::from_str("TypeB");

    store
        .append(
            agg.borrow(),
            vec![
                make_event(1, type_a, b"a1"),
                make_event(2, type_b, b"b1"),
                make_event(3, type_a, b"a2"),
            ],
        )
        .await
        .expect("append should succeed");

    // When: loading by TypeA only
    let stream = store
        .load(LoadQuery::by_event_type(type_a))
        .await
        .expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");

    // Then: only TypeA events are returned
    assert_eq!(events.len(), 2);
    assert!(
        events.iter().all(|e| e.event_type == type_a),
        "all returned events must have event_type TypeA"
    );
}

/// limit が指定された場合、返る Event 数が limit を超えない
#[tokio::test]
async fn load_with_limit_returns_at_most_limit_events() {
    // Given: one aggregate with five events
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");

    store
        .append(
            agg.borrow(),
            vec![
                make_event(1, et, b"1"),
                make_event(2, et, b"2"),
                make_event(3, et, b"3"),
                make_event(4, et, b"4"),
                make_event(5, et, b"5"),
            ],
        )
        .await
        .expect("append should succeed");

    // When: loading with limit=3
    let query = LoadQuery::by_stream(&agg).with_limit(3);
    let stream = store.load(query).await.expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");

    // Then: at most 3 events are returned
    assert_eq!(events.len(), 3);
}

/// 同一 Aggregate への sequence 重複は AppendError::SequenceConflict を返す
#[tokio::test]
async fn duplicate_sequence_on_same_aggregate_returns_sequence_conflict() {
    // Given: an aggregate with one event already appended
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");

    store
        .append(agg.borrow(), vec![make_event(1, et, b"first")])
        .await
        .expect("first append should succeed");

    // When: appending another event with the same sequence number
    let result = store
        .append(agg.borrow(), vec![make_event(1, et, b"conflict")])
        .await;

    // Then: SequenceConflict error is returned
    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict, got: {:?}",
        result
    );
}

/// バッチ内の sequence 衝突は all-or-nothing — global_sequence を消費しない
#[tokio::test]
async fn batch_append_with_conflict_is_all_or_nothing() {
    // Given: an aggregate with sequence 1 already stored
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");

    let outcome = store
        .append(agg.borrow(), vec![make_event(1, et, b"existing")])
        .await
        .expect("first append should succeed");
    let last_global_seq = outcome.assigned_sequences[0];

    // When: a batch containing a conflicting sequence is appended
    let result = store
        .append(
            agg.borrow(),
            vec![make_event(2, et, b"ok"), make_event(1, et, b"conflict")],
        )
        .await;

    // Then: the whole batch fails and no global_sequences are advanced
    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for conflicting batch"
    );

    // Confirm global counter was not advanced: a fresh append gets the next seq
    let agg2 = AggregateId::new("agg-2");
    let outcome2 = store
        .append(agg2.borrow(), vec![make_event(1, et, b"next")])
        .await
        .expect("append agg2 should succeed");
    assert_eq!(
        outcome2.assigned_sequences[0],
        last_global_seq + 1,
        "global_sequence must not have been consumed by the failed batch"
    );
}

/// 空 Aggregate への load は空 Stream を返す（エラーにならない）
#[tokio::test]
async fn load_nonexistent_aggregate_returns_empty_stream() {
    // Given: an empty store
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("nonexistent");

    // When: loading events for an aggregate that has none
    let stream = store
        .load(LoadQuery::by_stream(&agg))
        .await
        .expect("load should succeed even for unknown aggregate");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");

    // Then: an empty list is returned
    assert!(events.is_empty());
}

/// 同一バッチ内に重複 sequence が含まれる場合は SequenceConflict を返し、何も挿入しない
#[tokio::test]
async fn intra_batch_duplicate_sequence_returns_sequence_conflict() {
    // Given: an empty store
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");

    // When: a batch containing two events with the same sequence is appended
    let result = store
        .append(
            agg.borrow(),
            vec![make_event(5, et, b"first"), make_event(5, et, b"duplicate")],
        )
        .await;

    // Then: SequenceConflict is returned and no event is stored
    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for intra-batch duplicate sequence, got: {:?}",
        result
    );
    // Confirm the stream remains empty
    let stream = store
        .load(LoadQuery::by_stream(&agg))
        .await
        .expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");
    assert!(
        events.is_empty(),
        "no event must be stored after intra-batch conflict"
    );
}

/// 空バッチの append は no-op — assigned_sequences が空で stream_version は現在の最大 sequence を返す
#[tokio::test]
async fn empty_batch_append_is_noop_and_returns_current_stream_version() {
    // Given: an aggregate with one event already stored (sequence=3)
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::from_str("TestEvent");

    store
        .append(agg.borrow(), vec![make_event(3, et, b"existing")])
        .await
        .expect("initial append should succeed");

    // When: an empty batch is appended
    let outcome = store
        .append(agg.borrow(), vec![])
        .await
        .expect("empty batch append should succeed");

    // Then: no sequences are assigned, stream_version equals the current max (3)
    assert!(
        outcome.assigned_sequences.is_empty(),
        "empty batch must assign no sequences"
    );
    assert_eq!(
        outcome.stream_version, 3,
        "stream_version must equal the current max sequence"
    );

    // And the stored event is untouched
    let stream = store
        .load(LoadQuery::by_stream(&agg))
        .await
        .expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");
    assert_eq!(events.len(), 1, "no event must be added by empty batch");
}

/// 空バッチを未存在 Aggregate に append した場合 stream_version は 0 を返す
#[tokio::test]
async fn empty_batch_on_nonexistent_aggregate_returns_stream_version_zero() {
    // Given: an empty store
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("new-agg");

    // When: an empty batch is appended to a never-written aggregate
    let outcome = store
        .append(agg.borrow(), vec![])
        .await
        .expect("empty batch on new aggregate should succeed");

    // Then: stream_version is 0 (no events exist)
    assert!(outcome.assigned_sequences.is_empty());
    assert_eq!(
        outcome.stream_version, 0,
        "stream_version must be 0 for a new aggregate"
    );
}
