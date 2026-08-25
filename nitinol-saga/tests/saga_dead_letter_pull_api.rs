//! Saga DLQ **pull** API: `DeadLetterQueue::{list, mark_processed, evict}`.
//!
//! The push path (`with_dead_letter_subscriber`) delivers dead letters as an
//! observability signal; this file pins the operator-facing *recovery* path.
//! Everything is observed through the EventStore — dead letters are seeded
//! directly onto the saga's own stream and the queue is driven over that store
//! — so no saga process needs to be resident, which is precisely the
//! situation an operator triaging a stopped saga is in.
//!
//! Wire contract pinned here:
//!
//! | operation        | appended event type                             |
//! |------------------|-------------------------------------------------|
//! | `mark_processed` | `nitinol.saga.dead_letter_disposition.processed` |
//! | `evict`          | `nitinol.saga.dead_letter_disposition.evicted`   |
//!
//! `evict` is a **logical** delete: the marker is appended, the original
//! `DeadLetterEvent` record stays in the store untouched.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;

use nitinol_eventsource::{appending_system_event, SystemEvent};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_saga::{
    DeadLetterEntry, DeadLetterEvent, DeadLetterQuery, DeadLetterQueue, DeadLetterQueueError,
    SagaFailure, SagaId, SourceContext,
};

// Wire identities observed in this file.

/// Type-level identity of a dead letter — the record the pull API lists.
const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

/// Type-level identity of a disposition marker — the record `mark_processed`
/// and `evict` append.  A **sibling** of `nitinol.saga.dead_letter`, never a
/// descendant of it: the existing dead-letter family prefix must keep matching
/// exactly the records it matched before this API existed.
const DISPOSITION_TYPE: EventType = EventType::new(
    Family::new("nitinol.saga"),
    TypeName::new("dead_letter_disposition"),
);

/// A user event type that is not a dead letter, used to prove
/// `mark_processed` / `evict` refuse a sequence that holds something else.
const NOTE_TYPE: EventType = EventType::new(Family::new("pull_it"), TypeName::new("Note"));

// Helpers

/// Seed one dead letter at `sequence` on the saga's own stream.
///
/// `DeadLetterEvent::seq` is written to equal `sequence` because the
/// production writer (`enqueue_dead_letter`) appends the event at exactly the
/// sequence it stamps into the payload.
async fn seed_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: u64,
    failure: SagaFailure,
    source: SourceContext,
) {
    let event = DeadLetterEvent {
        seq: sequence,
        saga_id: saga_id.clone(),
        failure,
        occurred_at_unix_millis: 1_700_000_000_000 + sequence as i64,
        source,
    };
    store
        .append(
            saga_id.as_str(),
            vec![appending_system_event(
                sequence,
                &event,
                jiff::Timestamp::now(),
            )],
        )
        .await
        .expect("seeding a dead letter must succeed");
}

/// Seed a plain dead letter whose only distinguishing mark is `error`.
async fn seed_handle_failure(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
    seed_dead_letter(
        store,
        saga_id,
        sequence,
        SagaFailure::HandleFailed {
            error: format!("failure-{sequence}"),
        },
        SourceContext::without_upstream(),
    )
    .await;
}

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn of_type(events: &[LoadedEvent], event_type: EventType) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| e.event_type.type_key() == event_type.type_key())
        .collect()
}

fn count_variant(events: &[LoadedEvent], event_type: EventType, variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == event_type.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
        .count()
}

fn sequences(entries: &[DeadLetterEntry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.sequence).collect()
}

async fn list(queue: &DeadLetterQueue, query: DeadLetterQuery) -> Vec<DeadLetterEntry> {
    queue.list(query).await.expect("list must succeed")
}

// Test 1 — the operator round trip: list → mark_processed → evict

/// Given a saga stream carrying three dead letters,
/// When they are listed, one is marked processed and another evicted,
/// Then `list` returns every unprocessed dead letter and drops exactly the
/// marked ones.
///
/// The listed entry also carries the *recovery material*: the failure's raw
/// `message` bytes and the `SourceContext` coordinates.  Returning only the
/// diagnostic `error` string would leave the downstream application with
/// nothing to reprocess from.
#[tokio::test]
async fn list_returns_unprocessed_dead_letters_and_drops_marked_and_evicted_ones() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("pull-roundtrip-saga");

    seed_handle_failure(&store, &saga_id, 1).await;
    seed_dead_letter(
        &store,
        &saga_id,
        2,
        SagaFailure::EndedSagaReceivedMessage {
            message: Bytes::from_static(b"recovery-payload"),
        },
        SourceContext {
            aggregate_id: AggregateId::new("order-7"),
            upstream_sequence: 42,
        },
    )
    .await;
    seed_handle_failure(&store, &saga_id, 3).await;

    let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());

    let all = list(&queue, DeadLetterQuery::default()).await;
    assert_eq!(
        sequences(&all),
        vec![1, 2, 3],
        "an unmarked stream must list every dead letter, in stream order"
    );

    // Recovery material: `message` bytes + source coordinates, not just the
    // diagnostic string.
    let entry = &all[1];
    assert_eq!(entry.sequence, 2);
    match &entry.event.failure {
        SagaFailure::EndedSagaReceivedMessage { message } => assert_eq!(
            message,
            &Bytes::from_static(b"recovery-payload"),
            "the entry must carry the raw payload the downstream application \
             reprocesses from"
        ),
        other => panic!("expected EndedSagaReceivedMessage, got {other:?}"),
    }
    assert_eq!(
        entry.event.source.aggregate_id,
        AggregateId::new("order-7"),
        "the entry must carry the upstream aggregate coordinate"
    );
    assert_eq!(
        entry.event.source.upstream_sequence, 42,
        "the entry must carry the upstream sequence coordinate"
    );

    queue
        .mark_processed(1)
        .await
        .expect("mark_processed on a real dead letter must succeed");
    assert_eq!(
        sequences(&list(&queue, DeadLetterQuery::default()).await),
        vec![2, 3],
        "a processed dead letter must leave the list; the untouched ones must stay"
    );

    queue
        .evict(3)
        .await
        .expect("evict on a real dead letter must succeed");
    assert_eq!(
        sequences(&list(&queue, DeadLetterQuery::default()).await),
        vec![2],
        "an evicted dead letter must leave the list; the untouched one must stay"
    );
}

// Test 2 — the markers are real, appended, distinguishable events

/// Given two dead letters,
/// When one is marked processed and the other evicted,
/// Then two disposition markers are observable through `EventStore::load`,
/// each under its own per-arm wire variant, in a family that is a *sibling* of
/// the dead-letter family rather than a descendant of it.
///
/// The sibling clause is what keeps the existing dead-letter prefix query (and
/// with it the push subscriber, which decodes everything that prefix matches)
/// blind to the new markers.
#[tokio::test]
async fn mark_processed_and_evict_append_sibling_family_markers_to_the_store() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("pull-marker-saga");

    seed_handle_failure(&store, &saga_id, 1).await;
    seed_handle_failure(&store, &saga_id, 2).await;

    let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());
    queue.mark_processed(1).await.expect("mark_processed");
    queue.evict(2).await.expect("evict");

    let events = load_stream(&store, &saga_id).await;

    assert_eq!(
        count_variant(&events, DISPOSITION_TYPE, "processed"),
        1,
        "mark_processed must append exactly one `dead_letter_disposition.processed`"
    );
    assert_eq!(
        count_variant(&events, DISPOSITION_TYPE, "evicted"),
        1,
        "evict must append exactly one `dead_letter_disposition.evicted`"
    );

    let dispositions = of_type(&events, DISPOSITION_TYPE);
    assert_eq!(
        dispositions.len(),
        2,
        "exactly the two markers may be appended"
    );
    for marker in &dispositions {
        assert!(
            !marker
                .event_type
                .to_path()
                .is_within(&DEAD_LETTER_TYPE.to_path()),
            "a disposition marker ({}) must not fall under the \
             `nitinol.saga.dead_letter` prefix — that prefix is what the push \
             subscriber and the dead-letter query select on",
            marker.event_type
        );
    }

    assert_eq!(
        of_type(&events, DEAD_LETTER_TYPE).len(),
        2,
        "appending markers must not add to — or remove from — the dead letters"
    );
}

// Test 3 — evict is a logical delete, never a physical one

/// Given dead letters that are subsequently marked processed and evicted,
/// When the saga stream is loaded in full,
/// Then every original `DeadLetterEvent` record is still present, byte for
/// byte, and still decodes.
///
/// This is the clause that separates "removed from the operator's worklist"
/// from "removed from the log".  CQRS+ES has no second one.
#[tokio::test]
async fn marking_and_evicting_leave_the_original_dead_letter_records_intact() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("pull-no-delete-saga");

    seed_handle_failure(&store, &saga_id, 1).await;
    seed_handle_failure(&store, &saga_id, 2).await;
    seed_handle_failure(&store, &saga_id, 3).await;

    let before: Vec<(u64, EventType, Bytes)> =
        of_type(&load_stream(&store, &saga_id).await, DEAD_LETTER_TYPE)
            .into_iter()
            .map(|e| (e.sequence, e.event_type, e.payload.clone()))
            .collect();
    assert_eq!(before.len(), 3, "precondition: three dead letters seeded");

    let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());
    queue.mark_processed(1).await.expect("mark_processed");
    queue.evict(2).await.expect("evict");

    let events = load_stream(&store, &saga_id).await;
    let after: Vec<(u64, EventType, Bytes)> = of_type(&events, DEAD_LETTER_TYPE)
        .into_iter()
        .map(|e| (e.sequence, e.event_type, e.payload.clone()))
        .collect();

    assert_eq!(
        after, before,
        "every dead letter record must survive a disposition marker unchanged — \
         evict is a logical delete (marker append), never a physical one"
    );
    for record in of_type(&events, DEAD_LETTER_TYPE) {
        DeadLetterEvent::decode(&record.payload)
            .expect("a retained dead letter must still decode after being marked");
    }

    assert_eq!(
        sequences(&list(&queue, DeadLetterQuery::default()).await),
        vec![3],
        "the marked dead letters must be gone from the list even though they are \
         still in the store"
    );
}

// Test 4 — filtering and paging

/// Given five dead letters of which three are marked,
/// When `list` is called with `from_sequence` and `limit`,
/// Then `from_sequence` is an **inclusive** lower bound and `limit` caps the
/// number of *returned* entries — it is applied after marked entries are
/// excluded, not before.
///
/// Pushing `limit` down to the store would truncate the scan before the
/// exclusion, so `limit = 1` here would scan sequence 1 (marked) and return
/// nothing.  Pushing it down to the *disposition* scan would be worse: only
/// one of the three markers would be seen and an already-marked dead letter
/// would resurface.
#[tokio::test]
async fn from_sequence_is_inclusive_and_limit_caps_entries_after_exclusion() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("pull-paging-saga");

    for sequence in 1..=5 {
        seed_handle_failure(&store, &saga_id, sequence).await;
    }

    let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());
    queue.mark_processed(1).await.expect("mark_processed 1");
    queue.mark_processed(2).await.expect("mark_processed 2");
    queue.evict(3).await.expect("evict 3");
    // Unmarked: 4, 5.  Markers now occupy sequences 6, 7, 8.

    assert_eq!(
        sequences(&list(&queue, DeadLetterQuery::default()).await),
        vec![4, 5],
        "the default query must return every unmarked dead letter"
    );

    assert_eq!(
        sequences(
            &list(
                &queue,
                DeadLetterQuery {
                    from_sequence: Some(5),
                    ..Default::default()
                },
            )
            .await
        ),
        vec![5],
        "`from_sequence` must include the dead letter sitting exactly on the bound"
    );

    assert_eq!(
        sequences(
            &list(
                &queue,
                DeadLetterQuery {
                    from_sequence: Some(4),
                    ..Default::default()
                },
            )
            .await
        ),
        vec![4, 5],
        "`from_sequence` below the first unmarked entry must not drop it"
    );

    assert_eq!(
        sequences(
            &list(
                &queue,
                DeadLetterQuery {
                    from_sequence: Some(6),
                    ..Default::default()
                },
            )
            .await
        ),
        Vec::<u64>::new(),
        "sequences 6-8 hold disposition markers, not dead letters — the listing \
         must be empty rather than surfacing the markers as entries"
    );

    assert_eq!(
        sequences(
            &list(
                &queue,
                DeadLetterQuery {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await
        ),
        vec![4],
        "`limit` must cap the entries returned, so it has to be applied after the \
         marked entries are excluded"
    );

    assert_eq!(
        sequences(
            &list(
                &queue,
                DeadLetterQuery {
                    from_sequence: Some(5),
                    limit: Some(1),
                },
            )
            .await
        ),
        vec![5],
        "`from_sequence` and `limit` must compose"
    );
}

// Test 5 — a disposition may only be appended against a real dead letter

/// Given a stream holding one dead letter and one unrelated user event,
/// When `mark_processed` / `evict` name the user event's sequence, or a
/// sequence that holds nothing at all,
/// Then the call fails with `NotADeadLetter` and appends nothing.
///
/// A marker written against a sequence that is not a dead letter is
/// unresolvable forever: nothing downstream can tell what it disposed of.
#[tokio::test]
async fn disposition_is_refused_for_a_sequence_that_is_not_a_dead_letter() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("pull-guard-saga");

    seed_handle_failure(&store, &saga_id, 1).await;
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 2,
                event_type: NOTE_TYPE,
                payload: Bytes::from_static(b"a user event, not a dead letter"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a non-dead-letter event must succeed");

    let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());

    for sequence in [2, 99] {
        assert!(
            matches!(
                queue.mark_processed(sequence).await,
                Err(DeadLetterQueueError::NotADeadLetter { .. })
            ),
            "mark_processed({sequence}) must be refused: that sequence holds no dead letter"
        );
        assert!(
            matches!(
                queue.evict(sequence).await,
                Err(DeadLetterQueueError::NotADeadLetter { .. })
            ),
            "evict({sequence}) must be refused: that sequence holds no dead letter"
        );
    }

    assert_eq!(
        load_stream(&store, &saga_id).await.len(),
        2,
        "a refused disposition must append nothing at all"
    );

    // The guard rejects the wrong target, not every target.
    queue
        .mark_processed(1)
        .await
        .expect("the real dead letter must still be markable");
    assert_eq!(
        load_stream(&store, &saga_id).await.len(),
        3,
        "the accepted disposition must be the one and only appended marker"
    );
}

// Test 6 — the queue is scoped to its own saga

/// Given two sagas sharing one `EventStore`,
/// When each saga's queue is listed and marked,
/// Then neither sees nor disposes of the other's dead letters.
///
/// One physical store hosts every saga's stream side by side, so a `list` that
/// forgot its stream key would hand an operator another process's failures —
/// and `mark_processed` would silence them.
#[tokio::test]
async fn each_queue_sees_only_its_own_saga_stream() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_a = SagaId::new("pull-scope-saga-a");
    let saga_b = SagaId::new("pull-scope-saga-b");

    seed_handle_failure(&store, &saga_a, 1).await;
    seed_handle_failure(&store, &saga_b, 1).await;
    seed_handle_failure(&store, &saga_b, 2).await;

    let queue_a = DeadLetterQueue::new(Arc::clone(&store), saga_a.clone());
    let queue_b = DeadLetterQueue::new(Arc::clone(&store), saga_b.clone());

    assert_eq!(
        list(&queue_a, DeadLetterQuery::default()).await.len(),
        1,
        "saga A's queue must list only saga A's dead letter"
    );
    assert_eq!(
        sequences(&list(&queue_b, DeadLetterQuery::default()).await),
        vec![1, 2],
        "saga B's queue must list only saga B's dead letters"
    );

    queue_a
        .mark_processed(1)
        .await
        .expect("mark_processed on A");

    assert!(
        list(&queue_a, DeadLetterQuery::default()).await.is_empty(),
        "saga A's only dead letter must be marked"
    );
    assert_eq!(
        sequences(&list(&queue_b, DeadLetterQuery::default()).await),
        vec![1, 2],
        "saga B's dead letter at the same sequence must be untouched by A's marker"
    );
    assert_eq!(
        of_type(&load_stream(&store, &saga_b).await, DISPOSITION_TYPE).len(),
        0,
        "no marker may be written onto saga B's stream"
    );
}
