use std::borrow::Borrow;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadQuery, TypeName};

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
    let et = EventType::new(Family::new(""), TypeName::new("TestEvent"));
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

/// by_event_type フィルタで該当する EventType のみ返る
#[tokio::test]
async fn load_by_event_type_returns_only_matching_events() {
    // Given: one aggregate with three events of two different types
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let type_a = EventType::new(Family::new(""), TypeName::new("TypeA"));
    let type_b = EventType::new(Family::new(""), TypeName::new("TypeB"));

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
    let et = EventType::new(Family::new(""), TypeName::new("TestEvent"));

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

/// 空バッチの append は no-op — assigned_sequences が空で stream_version は現在の最大 sequence を返す
#[tokio::test]
async fn empty_batch_append_is_noop_and_returns_current_stream_version() {
    // Given: an aggregate with one event already stored (sequence=3)
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = EventType::new(Family::new(""), TypeName::new("TestEvent"));

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
