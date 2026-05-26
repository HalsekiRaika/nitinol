//! New EventStore key API tests (Issue #40).
//!
//! Verifies the redesigned event-store key abstraction:
//!
//! 1. `EventStore::append` accepts `&str` (not `&AggregateId`) so the trait
//!    remains dyn-compatible (`Arc<dyn EventStore>` keeps working).
//! 2. `AggregateId` implements `Borrow<str>`, so callers continue to write
//!    `store.append(id.borrow(), events)` ergonomically.
//! 3. `LoadQuery` exposes `stream_key: Option<String>` (renamed from
//!    `aggregate_id`) and `from_stream_sequence: Option<u64>` (renamed from
//!    `from_aggregate_sequence`), reflecting that the same store now serves
//!    both aggregates and sagas.
//! 4. `LoadQuery::by_stream` is generic over `K: Borrow<str>`, accepting
//!    `AggregateId`, `&str`, `String`, etc.
//! 5. `AppendingEvent` no longer carries a redundant `aggregate_id` field;
//!    the append() key is the sole source of truth for stream identity.
//! 6. `LoadedEvent.stream_key: String` replaces `aggregate_id: AggregateId`.
//! 7. `AppendError::SequenceConflict` carries `String` (not `AggregateId`).
//! 8. `AppendError::AggregateMismatch` is gone — the mismatch check is removed.
//!
//! Compile and pass after the EventStore refactor lands.

use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadQuery};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Constructs an `AppendingEvent` without an `aggregate_id` field — the key
/// is provided exclusively by `EventStore::append`.
fn make_event(sequence: u64, event_type: EventType, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

// ---------------------------------------------------------------------------
// AggregateId / Borrow<str>
// ---------------------------------------------------------------------------

/// AggregateId implements Borrow<str> so it can be passed wherever &str is
/// expected via `.borrow()`.
#[test]
fn aggregate_id_borrows_as_str() {
    // Given
    let id = AggregateId::new("agg-borrow");

    // When
    let borrowed: &str = id.borrow();

    // Then
    assert_eq!(borrowed, "agg-borrow");
}

// ---------------------------------------------------------------------------
// EventStore::append accepts &str
// ---------------------------------------------------------------------------

/// EventStore::append accepts a plain &str key (not &AggregateId).
#[tokio::test]
async fn append_accepts_str_key_directly() {
    // Given
    let store = InMemoryEventStore::default();
    let et = EventType::from_str("TestEvent");

    // When: append using a string slice directly
    let outcome = store
        .append("plain-str-key", vec![make_event(1, et, b"payload")])
        .await
        .expect("append must succeed with &str key");

    // Then
    assert_eq!(outcome.assigned_sequences.len(), 1);
    assert_eq!(outcome.stream_version, 1);
}

/// EventStore::append accepts an AggregateId borrowed as &str.
#[tokio::test]
async fn append_accepts_aggregate_id_via_borrow() {
    // Given
    let store = InMemoryEventStore::default();
    let id = AggregateId::new("agg-borrow-append");
    let et = EventType::from_str("TestEvent");

    // When: pass through Borrow<str>
    let outcome = store
        .append(id.borrow(), vec![make_event(1, et, b"payload")])
        .await
        .expect("append must succeed via Borrow<str>");

    // Then
    assert_eq!(outcome.stream_version, 1);
}

// ---------------------------------------------------------------------------
// LoadQuery::by_stream
// ---------------------------------------------------------------------------

/// LoadQuery::by_stream accepts a &str literal and produces a query that
/// targets the corresponding stream.
#[tokio::test]
async fn load_by_stream_with_str_returns_matching_events() {
    // Given
    let store = InMemoryEventStore::default();
    let et = EventType::from_str("TestEvent");
    store
        .append("stream-a", vec![make_event(1, et, b"a1")])
        .await
        .expect("append stream-a must succeed");

    // When
    let stream = store
        .load(LoadQuery::by_stream("stream-a"))
        .await
        .expect("load must succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect must succeed");

    // Then
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stream_key, "stream-a");
}

/// LoadQuery::by_stream accepts an AggregateId via its Borrow<str> impl.
#[tokio::test]
async fn load_by_stream_with_aggregate_id_returns_matching_events() {
    // Given
    let store = InMemoryEventStore::default();
    let id = AggregateId::new("agg-by-stream");
    let et = EventType::from_str("TestEvent");
    store
        .append(id.borrow(), vec![make_event(1, et, b"payload")])
        .await
        .expect("append must succeed");

    // When: pass AggregateId where Borrow<str> is expected
    let stream = store
        .load(LoadQuery::by_stream(&id))
        .await
        .expect("load must succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect must succeed");

    // Then
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stream_key, "agg-by-stream");
}

// ---------------------------------------------------------------------------
// LoadQuery: stream_key / from_stream_sequence renamed fields
// ---------------------------------------------------------------------------

/// LoadQuery exposes `stream_key: Option<String>` and
/// `from_stream_sequence: Option<u64>` (renamed from `aggregate_id` /
/// `from_aggregate_sequence`).
#[tokio::test]
async fn load_query_uses_renamed_stream_fields() {
    // Given
    let store = InMemoryEventStore::default();
    let et = EventType::from_str("TestEvent");
    store
        .append(
            "stream-renamed",
            vec![
                make_event(1, et, b"e1"),
                make_event(2, et, b"e2"),
                make_event(3, et, b"e3"),
            ],
        )
        .await
        .expect("append must succeed");

    // When: build a query using the new field names directly
    let query = LoadQuery {
        stream_key: Some("stream-renamed".to_string()),
        from_stream_sequence: Some(2),
        ..Default::default()
    };
    let stream = store.load(query).await.expect("load must succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect must succeed");

    // Then
    assert_eq!(events.len(), 2, "from_stream_sequence=2 must yield 2 events");
    assert_eq!(events[0].sequence, 2);
    assert_eq!(events[1].sequence, 3);
}

// ---------------------------------------------------------------------------
// LoadedEvent.stream_key
// ---------------------------------------------------------------------------

/// LoadedEvent exposes `stream_key: String` (renamed from `aggregate_id`).
#[tokio::test]
async fn loaded_event_exposes_stream_key_string() {
    // Given
    let store = InMemoryEventStore::default();
    let et = EventType::from_str("TestEvent");
    store
        .append("loaded-stream-key", vec![make_event(1, et, b"x")])
        .await
        .expect("append must succeed");

    // When
    let stream = store
        .load(LoadQuery::by_stream("loaded-stream-key"))
        .await
        .expect("load must succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect must succeed");

    // Then
    assert_eq!(events.len(), 1);
    let key: &String = &events[0].stream_key;
    assert_eq!(key, "loaded-stream-key");
}

// ---------------------------------------------------------------------------
// SequenceConflict carries String
// ---------------------------------------------------------------------------

/// Sequence conflict returns `AppendError::SequenceConflict(String)` carrying
/// the stream key as a String.
#[tokio::test]
async fn sequence_conflict_error_carries_string_key() {
    // Given
    let store = InMemoryEventStore::default();
    let et = EventType::from_str("TestEvent");
    store
        .append("conflict-stream", vec![make_event(1, et, b"first")])
        .await
        .expect("first append must succeed");

    // When: append with the same sequence again
    let result = store
        .append("conflict-stream", vec![make_event(1, et, b"dup")])
        .await;

    // Then
    match result {
        Err(AppendError::SequenceConflict(key)) => {
            assert_eq!(
                key.as_str(),
                "conflict-stream",
                "SequenceConflict must carry the stream key as a String"
            );
        }
        Ok(_) => panic!("append must fail with SequenceConflict"),
        Err(other) => panic!("unexpected error variant: {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Empty batch on the new key shape still works
// ---------------------------------------------------------------------------

/// Empty batches remain a no-op under the new &str-key API.
#[tokio::test]
async fn empty_batch_with_str_key_is_noop() {
    // Given
    let store = InMemoryEventStore::default();

    // When
    let outcome = store
        .append("noop-stream", vec![])
        .await
        .expect("empty-batch append must succeed");

    // Then
    assert!(outcome.assigned_sequences.is_empty());
    assert_eq!(outcome.stream_version, 0);
}

// ---------------------------------------------------------------------------
// Arc<dyn EventStore>: the trait remains dyn-compatible after the refactor
// ---------------------------------------------------------------------------

/// EventStore is dyn-compatible — the change to `&str`-keyed methods must NOT
/// regress the ability to use `Arc<dyn EventStore>`.  This is the central
/// design constraint that justifies `&str` over `<K: Borrow<str>>` generics.
#[tokio::test]
async fn event_store_is_dyn_compatible() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let et = EventType::from_str("TestEvent");

    // When
    store
        .append("dyn-stream", vec![make_event(1, et, b"payload")])
        .await
        .expect("dyn append must succeed");

    let stream = store
        .load(LoadQuery::by_stream("dyn-stream"))
        .await
        .expect("dyn load must succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect must succeed");

    // Then
    assert_eq!(events.len(), 1);
}

// ---------------------------------------------------------------------------
// Same EventStore serves multiple stream-key types side-by-side
// ---------------------------------------------------------------------------

/// AggregateId and a plain &str saga key can append to the same InMemoryEventStore
/// without interfering — both are first-class stream keys via Borrow<str>.
/// This is the scenario the Borrow<str> abstraction was introduced to support
/// (`SagaId` and `AggregateId` share one physical store, per F-21 (a)).
#[tokio::test]
async fn distinct_stream_keys_coexist_in_one_store() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let agg = AggregateId::new("coexist-agg");
    let saga_key: &str = "coexist-saga";
    let et = EventType::from_str("TestEvent");

    // When
    store
        .append(agg.borrow(), vec![make_event(1, et, b"agg-1")])
        .await
        .expect("append aggregate stream must succeed");
    store
        .append(saga_key, vec![make_event(1, et, b"saga-1")])
        .await
        .expect("append saga stream must succeed");

    // Then: each stream is loaded independently
    let agg_events: Vec<_> = store
        .load(LoadQuery::by_stream(&agg))
        .await
        .expect("load agg must succeed")
        .try_collect()
        .await
        .expect("collect agg must succeed");
    let saga_events: Vec<_> = store
        .load(LoadQuery::by_stream(saga_key))
        .await
        .expect("load saga must succeed")
        .try_collect()
        .await
        .expect("collect saga must succeed");

    assert_eq!(agg_events.len(), 1);
    assert_eq!(agg_events[0].stream_key, "coexist-agg");
    assert_eq!(saga_events.len(), 1);
    assert_eq!(saga_events[0].stream_key, "coexist-saga");
}
