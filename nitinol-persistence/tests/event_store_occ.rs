//! Optimistic-concurrency-control contract tests for [`InMemoryEventStore`].
//!
//! These tests are the normative record of the OCC behaviour every
//! `EventStore` backend must reproduce.  Each contract carries a stable label
//! that its test names begin with:
//!
//! | Label | Contract |
//! |---|---|
//! | `OCC-1` | `unique(stream, sequence)`: appending a sequence that already exists in the stream, or twice within one batch, fails with [`AppendError::SequenceConflict`]. |
//! | `OCC-2` | Idempotent genesis: re-appending the genesis sequence also fails with `SequenceConflict`, and the first write's content, count, and assigned `global_sequence` stay untouched. |
//! | `OCC-3` | Atomic visibility and monotonicity: `append(Vec)` commits all-or-nothing, a conflicting append consumes no `global_sequence`, and `load` yields events in ascending `global_sequence` order. |
//! | `OCC-4` | No internal retry: a sequence conflict is reported immediately, leaving every observable byte of the store — stored events and `global_sequence` numbering — exactly as it was. |
//!
//! A stream's genesis sequence is `1`: producers start their counter at `0`
//! and increment before assigning, so the first event ever appended to a
//! stream carries `sequence = 1`.
//!
//! Only deterministic invariants are fixed here. No test races concurrent
//! readers or writers against each other.

use std::borrow::Borrow;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::error::AppendError;
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

/// Every field of a stored event, in a shape that can be compared for
/// equality. `LoadedEvent` deliberately does not derive `PartialEq`, so the
/// "observable state is unchanged" contract is expressed here instead of by
/// widening the crate's public API.
#[derive(Debug, PartialEq)]
struct StoredEvent {
    stream_key: String,
    sequence: u64,
    global_sequence: u64,
    event_type: EventType,
    payload: Bytes,
    occurred_at: Timestamp,
}

/// Full observable content of the store, in ascending `global_sequence` order.
async fn observe_all(store: &InMemoryEventStore) -> Vec<StoredEvent> {
    observe(store, LoadQuery::default()).await
}

async fn observe(store: &InMemoryEventStore, query: LoadQuery) -> Vec<StoredEvent> {
    let stream = store.load(query).await.expect("load should succeed");
    let loaded: Vec<_> = stream.try_collect().await.expect("collect should succeed");
    loaded
        .into_iter()
        .map(|event| StoredEvent {
            stream_key: event.stream_key,
            sequence: event.sequence,
            global_sequence: event.global_sequence,
            event_type: event.event_type,
            payload: event.payload,
            occurred_at: event.occurred_at,
        })
        .collect()
}

fn test_event_type() -> EventType {
    EventType::new(Family::new(""), TypeName::new("TestEvent"))
}

/// OCC-1: appending a sequence already present in the stream is rejected.
#[tokio::test]
async fn occ_1_duplicate_sequence_on_same_stream_returns_sequence_conflict() {
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = test_event_type();

    store
        .append(agg.borrow(), vec![make_event(1, et, b"first")])
        .await
        .expect("first append should succeed");

    let result = store
        .append(agg.borrow(), vec![make_event(1, et, b"conflict")])
        .await;

    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict, got: {result:?}"
    );
}

/// OCC-1: two events carrying the same sequence inside one batch are rejected
/// before anything is stored.
#[tokio::test]
async fn occ_1_intra_batch_duplicate_sequence_returns_sequence_conflict() {
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = test_event_type();

    let result = store
        .append(
            agg.borrow(),
            vec![make_event(5, et, b"first"), make_event(5, et, b"duplicate")],
        )
        .await;

    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for intra-batch duplicate sequence, got: {result:?}"
    );
    let stored = observe(&store, LoadQuery::by_stream(&agg)).await;
    assert!(
        stored.is_empty(),
        "no event must be stored after intra-batch conflict, got: {stored:?}"
    );
}

/// OCC-2: a second append at the genesis sequence conflicts and leaves the
/// first write — content, count, and assigned `global_sequence` — intact.
#[tokio::test]
async fn occ_2_duplicate_genesis_append_preserves_first_write() {
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let created = EventType::new(Family::new(""), TypeName::new("Created"));
    let genesis = make_event(1, created, b"genesis-original");
    let genesis_occurred_at = genesis.occurred_at;

    let outcome = store
        .append(agg.borrow(), vec![genesis])
        .await
        .expect("genesis append should succeed");
    let genesis_global_sequence = outcome.assigned_sequences[0];

    let renamed = EventType::new(Family::new(""), TypeName::new("Renamed"));
    let result = store
        .append(agg.borrow(), vec![make_event(1, renamed, b"genesis-retry")])
        .await;

    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for duplicate genesis append, got: {result:?}"
    );

    let stored = observe_all(&store).await;
    assert_eq!(
        stored.len(),
        1,
        "the conflicting retry must not add an event, got: {stored:?}"
    );
    assert_eq!(
        stored[0],
        StoredEvent {
            stream_key: agg.as_str().to_owned(),
            sequence: 1,
            global_sequence: genesis_global_sequence,
            event_type: created,
            payload: Bytes::from_static(b"genesis-original"),
            occurred_at: genesis_occurred_at,
        },
        "the first genesis write must survive the conflicting retry unchanged"
    );
}

/// OCC-3: a conflicting append consumes no `global_sequence`, so the next
/// successful append continues the numbering without a gap.
#[tokio::test]
async fn occ_3_conflict_does_not_advance_global_sequence_numbering() {
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = test_event_type();

    store
        .append(agg.borrow(), vec![make_event(1, et, b"e1")])
        .await
        .expect("append of sequence 1 should succeed");
    let outcome = store
        .append(agg.borrow(), vec![make_event(2, et, b"e2")])
        .await
        .expect("append of sequence 2 should succeed");
    let last_global_sequence = outcome.assigned_sequences[0];

    let result = store
        .append(agg.borrow(), vec![make_event(2, et, b"conflict")])
        .await;
    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for the colliding append, got: {result:?}"
    );

    let outcome = store
        .append(
            agg.borrow(),
            vec![make_event(3, et, b"e3"), make_event(4, et, b"e4")],
        )
        .await
        .expect("append after the conflict should succeed");
    assert_eq!(
        outcome.assigned_sequences,
        vec![last_global_sequence + 1, last_global_sequence + 2],
        "the failed append must not have consumed any global_sequence"
    );
}

/// OCC-3: a batch that contains one fresh and one colliding sequence is
/// rejected whole — neither event becomes visible and no numbering is spent.
#[tokio::test]
async fn occ_3_batch_conflict_is_all_or_nothing_and_consumes_no_global_sequence() {
    let store = InMemoryEventStore::default();
    let agg = AggregateId::new("agg-1");
    let et = test_event_type();

    let outcome = store
        .append(agg.borrow(), vec![make_event(1, et, b"existing")])
        .await
        .expect("first append should succeed");
    let last_global_seq = outcome.assigned_sequences[0];

    let result = store
        .append(
            agg.borrow(),
            vec![make_event(2, et, b"ok"), make_event(1, et, b"conflict")],
        )
        .await;

    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for conflicting batch, got: {result:?}"
    );
    let stored = observe(&store, LoadQuery::by_stream(&agg)).await;
    assert_eq!(
        stored.len(),
        1,
        "only the pre-existing event may remain visible, got: {stored:?}"
    );
    assert_eq!(
        stored[0].payload,
        Bytes::from_static(b"existing"),
        "the non-colliding event of the failed batch must not be visible"
    );

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

/// OCC-3: `load` orders events by `global_sequence`, i.e. by append order,
/// regardless of which stream each event belongs to.
#[tokio::test]
async fn occ_3_load_returns_events_in_ascending_global_sequence_order() {
    let store = InMemoryEventStore::default();
    let agg1 = AggregateId::new("agg-1");
    let agg2 = AggregateId::new("agg-2");
    let et = test_event_type();

    store
        .append(agg1.borrow(), vec![make_event(1, et, b"a1e1")])
        .await
        .expect("append agg1 e1 should succeed");
    store
        .append(agg2.borrow(), vec![make_event(7, et, b"a2e1")])
        .await
        .expect("append agg2 e1 should succeed");
    store
        .append(agg1.borrow(), vec![make_event(2, et, b"a1e2")])
        .await
        .expect("append agg1 e2 should succeed");

    let stored = observe(&store, LoadQuery::from_global(1)).await;

    assert_eq!(stored.len(), 3);
    let global_seqs: Vec<u64> = stored.iter().map(|e| e.global_sequence).collect();
    let mut sorted = global_seqs.clone();
    sorted.sort_unstable();
    assert_eq!(
        global_seqs, sorted,
        "events must be returned in global_sequence order"
    );
    let payloads: Vec<&[u8]> = stored.iter().map(|e| e.payload.as_ref()).collect();
    assert_eq!(
        payloads,
        vec![b"a1e1".as_slice(), b"a2e1".as_slice(), b"a1e2".as_slice()],
        "global_sequence order must reproduce the order the events were appended in"
    );
}

/// OCC-4: a conflict is answered immediately, with no internal retry — the
/// store's whole observable content is identical before and after the call.
#[tokio::test]
async fn occ_4_conflicting_append_leaves_observable_state_unchanged() {
    let store = InMemoryEventStore::default();
    let agg1 = AggregateId::new("agg-1");
    let agg2 = AggregateId::new("agg-2");
    let et = test_event_type();

    store
        .append(
            agg1.borrow(),
            vec![make_event(1, et, b"a1e1"), make_event(2, et, b"a1e2")],
        )
        .await
        .expect("seeding agg1 should succeed");
    store
        .append(agg2.borrow(), vec![make_event(1, et, b"a2e1")])
        .await
        .expect("seeding agg2 should succeed");

    let before = observe_all(&store).await;
    assert_eq!(
        before.len(),
        3,
        "the comparison must run against a non-empty store"
    );

    let result = store
        .append(
            agg1.borrow(),
            vec![
                make_event(3, et, b"occ4-fresh"),
                make_event(1, et, b"occ4-collision"),
            ],
        )
        .await;

    assert!(
        matches!(result, Err(AppendError::SequenceConflict(_))),
        "expected SequenceConflict for the mixed batch, got: {result:?}"
    );
    let after = observe_all(&store).await;
    assert_eq!(
        after, before,
        "a conflicting append must leave the observable store byte-for-byte unchanged"
    );

    let outcome = store
        .append(agg2.borrow(), vec![make_event(2, et, b"a2e2")])
        .await
        .expect("append after the conflict should succeed");
    assert_eq!(
        outcome.assigned_sequences,
        vec![before[2].global_sequence + 1],
        "the conflicting append must not have spent a global_sequence"
    );
}
