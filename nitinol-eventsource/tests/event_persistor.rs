// Integration tests for EventPersistor actor.
//
// EventPersistor is a dedicated persistence actor that owns Arc<dyn EventStore>.
// AggregateProcess communicates with it via EventPersistorRef, never holding the
// store directly — satisfying the Phase 2 lockless messaging design.
//
// Compile errors are expected until EventPersistor and EventPersistorRef are
// implemented in nitinol-eventsource/src/process/persistence/.

use std::sync::Arc;

use nitinol_eventsource::{EventPersistor, EventPersistorRef};
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadQuery};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Spawns a fresh ProcessSystem with an InMemoryEventStore-backed EventPersistor.
async fn setup() -> (ProcessSystem, EventPersistorRef) {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let event_ref = EventPersistor::spawn(&system, store).await;
    (system, event_ref)
}

fn appending_event(aggregate_id: &AggregateId, sequence: u64) -> AppendingEvent {
    AppendingEvent {
        aggregate_id: aggregate_id.clone(),
        sequence,
        event_type: EventType::from_str("TestEvent"),
        payload: bytes::Bytes::new(),
        occurred_at: jiff::Timestamp::now(),
    }
}

// ---------------------------------------------------------------------------
// AppendEvents: 正常系
// ---------------------------------------------------------------------------

/// AppendEvents メッセージが成功する。
#[tokio::test]
async fn event_persistor_append_events_succeeds() {
    // Given
    let (_system, event_ref) = setup().await;
    let id = AggregateId::new("ep-append-ok");
    let events = vec![appending_event(&id, 1)];

    // When
    let result = event_ref.append(id, events).await;

    // Then
    result.expect("append must succeed");
}

// ---------------------------------------------------------------------------
// LoadEvents: 追記後にロードで同じイベントが戻る
// ---------------------------------------------------------------------------

/// AppendEvents で追記した 1 件が LoadEvents で取得できる。
#[tokio::test]
async fn event_persistor_load_returns_appended_events() {
    // Given
    let (_system, event_ref) = setup().await;
    let id = AggregateId::new("ep-load-returns");
    event_ref
        .append(id.clone(), vec![appending_event(&id, 1)])
        .await
        .expect("append must succeed");

    // When
    let query = LoadQuery {
        aggregate_id: Some(id.clone()),
        ..Default::default()
    };
    let loaded = event_ref.load(query).await.expect("load must succeed");

    // Then
    assert_eq!(loaded.len(), 1, "load must return exactly 1 event");
    assert_eq!(loaded[0].sequence, 1, "loaded event must carry sequence=1");
    assert_eq!(
        loaded[0].aggregate_id, id,
        "loaded event must carry correct aggregate_id"
    );
}

// ---------------------------------------------------------------------------
// LoadEvents: 空ストアは空の Vec を返す
// ---------------------------------------------------------------------------

/// イベントが存在しない aggregate_id に対して LoadEvents は空の Vec を返す。
#[tokio::test]
async fn event_persistor_load_from_empty_store_returns_empty_vec() {
    // Given
    let (_system, event_ref) = setup().await;

    // When
    let query = LoadQuery {
        aggregate_id: Some(AggregateId::new("ep-load-empty")),
        ..Default::default()
    };
    let loaded = event_ref.load(query).await.expect("load must succeed");

    // Then
    assert!(
        loaded.is_empty(),
        "load from store with no events must return empty vec"
    );
}

// ---------------------------------------------------------------------------
// AppendEvents: 複数回追記が順序通り累積される
// ---------------------------------------------------------------------------

/// 3 回の AppendEvents がそれぞれ 1 件を追記し、LoadEvents で計 3 件が返る。
#[tokio::test]
async fn event_persistor_sequential_appends_accumulate() {
    // Given
    let (_system, event_ref) = setup().await;
    let id = AggregateId::new("ep-seq-append");

    // When: 3 consecutive appends
    event_ref
        .append(id.clone(), vec![appending_event(&id, 1)])
        .await
        .expect("append 1 must succeed");
    event_ref
        .append(id.clone(), vec![appending_event(&id, 2)])
        .await
        .expect("append 2 must succeed");
    event_ref
        .append(id.clone(), vec![appending_event(&id, 3)])
        .await
        .expect("append 3 must succeed");

    // Then
    let query = LoadQuery {
        aggregate_id: Some(id),
        ..Default::default()
    };
    let loaded = event_ref.load(query).await.expect("load must succeed");
    assert_eq!(
        loaded.len(),
        3,
        "three sequential appends must result in three loaded events"
    );
    assert_eq!(loaded[0].sequence, 1);
    assert_eq!(loaded[1].sequence, 2);
    assert_eq!(loaded[2].sequence, 3);
}

// ---------------------------------------------------------------------------
// LoadEvents: from_aggregate_sequence フィルタが正しく適用される
// ---------------------------------------------------------------------------

/// from_aggregate_sequence=2 のクエリは sequence >= 2 のイベントだけを返す。
#[tokio::test]
async fn event_persistor_load_respects_sequence_filter() {
    // Given: append events at sequence 1, 2, 3 in a single batch
    let (_system, event_ref) = setup().await;
    let id = AggregateId::new("ep-seq-filter");
    event_ref
        .append(
            id.clone(),
            vec![
                appending_event(&id, 1),
                appending_event(&id, 2),
                appending_event(&id, 3),
            ],
        )
        .await
        .expect("append must succeed");

    // When: load only from sequence 2
    let query = LoadQuery {
        aggregate_id: Some(id),
        from_aggregate_sequence: Some(2),
        ..Default::default()
    };
    let loaded = event_ref.load(query).await.expect("load must succeed");

    // Then: only events with sequence >= 2 are returned
    assert_eq!(
        loaded.len(),
        2,
        "loading from sequence=2 must return 2 events (seq=2 and seq=3)"
    );
    assert_eq!(loaded[0].sequence, 2, "first loaded event must have sequence=2");
    assert_eq!(loaded[1].sequence, 3, "second loaded event must have sequence=3");
}

// ---------------------------------------------------------------------------
// LoadEvents: 異なる aggregate_id のイベントが混在しない
// ---------------------------------------------------------------------------

/// 2 つの aggregate_id に追記した後、それぞれの LoadEvents が混在しない。
#[tokio::test]
async fn event_persistor_load_isolates_by_aggregate_id() {
    // Given: append 1 event to id_a and 2 events to id_b
    let (_system, event_ref) = setup().await;
    let id_a = AggregateId::new("ep-isolate-a");
    let id_b = AggregateId::new("ep-isolate-b");

    event_ref
        .append(id_a.clone(), vec![appending_event(&id_a, 1)])
        .await
        .expect("append id_a must succeed");
    event_ref
        .append(
            id_b.clone(),
            vec![appending_event(&id_b, 1), appending_event(&id_b, 2)],
        )
        .await
        .expect("append id_b must succeed");

    // When
    let loaded_a = event_ref
        .load(LoadQuery {
            aggregate_id: Some(id_a.clone()),
            ..Default::default()
        })
        .await
        .expect("load id_a must succeed");
    let loaded_b = event_ref
        .load(LoadQuery {
            aggregate_id: Some(id_b.clone()),
            ..Default::default()
        })
        .await
        .expect("load id_b must succeed");

    // Then
    assert_eq!(loaded_a.len(), 1, "id_a must have exactly 1 event");
    assert_eq!(loaded_b.len(), 2, "id_b must have exactly 2 events");
    assert!(
        loaded_a.iter().all(|e| e.aggregate_id == id_a),
        "all id_a events must belong to id_a"
    );
    assert!(
        loaded_b.iter().all(|e| e.aggregate_id == id_b),
        "all id_b events must belong to id_b"
    );
}
