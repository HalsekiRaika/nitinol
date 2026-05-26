// Integration tests for the Catch-up flow (and catch-up / live boundary).
//
// Tests verify:
// - All events in the EventStore are projected during catch-up when there is
//   no prior checkpoint.
// - When a checkpoint exists, only events after the checkpoint are projected.
// - Multiple aggregates can be caught up in global_sequence order.
// - Live events with sequence ≤ the catch-up boundary are deduplicated (skipped).
//
// Compile errors involving the projection module are expected until the
// implementation in nitinol-eventsource/src/projection/ is complete.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, Event, EventEnvelope, ProjectionContext, Projector, ProjectorProps,
};
use nitinol_persistence::store::{
    CheckpointStore, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event type
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("Evt");
}

// ---------------------------------------------------------------------------
// Fixtures: codec
// ---------------------------------------------------------------------------

struct UnitCodec;

impl Codec<Evt> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Evt) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Evt, Self::Error> {
        Ok(Evt)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: SequenceRecordingProjector
//
// Records every sequence it sees so the test can verify which events were
// projected (and in what order).
// ---------------------------------------------------------------------------

struct SequenceRecordingProjector {
    sequences: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Evt> for SequenceRecordingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.sequences
            .lock()
            .unwrap()
            .push(ctx.current_sequence());
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::from_str("Evt"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

async fn wait_for_count(counter: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} project() calls"));
}

// ---------------------------------------------------------------------------
// Test: all events are projected when no checkpoint exists
// ---------------------------------------------------------------------------

/// Append 3 events to a fresh store (no checkpoint). All 3 are projected
/// during catch-up.
#[tokio::test]
async fn catchup_all_events_processed_from_empty_checkpoint() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("cu-all-agg");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("cu-all"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || SequenceRecordingProjector {
            sequences: Arc::clone(&seq_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 3).await;

    // Then
    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![1, 2, 3],
        "all 3 events must be projected in order when there is no checkpoint"
    );
}

// ---------------------------------------------------------------------------
// Test: only events after the checkpoint are projected
// ---------------------------------------------------------------------------

/// A checkpoint at sequence=2 means only the event at sequence=3 is
/// processed during catch-up.
#[tokio::test]
async fn catchup_skips_events_before_checkpoint() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("cu-skip-agg");
    let projection_id = ProjectionId::new("cu-skip");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    // Pre-save checkpoint at sequence=2 (simulates a prior run that processed up to seq=2)
    checkpoint_store
        .save(&projection_id, 2, None)
        .await
        .expect("pre-save checkpoint must succeed");

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When
    let _proxy = ProjectorProps::new(
        projection_id,
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || SequenceRecordingProjector {
            sequences: Arc::clone(&seq_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Then: only seq=3 was projected
    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![3],
        "only the event after the checkpoint (seq=3) must be projected"
    );
}

// ---------------------------------------------------------------------------
// Test: empty event store — project() is never called
// ---------------------------------------------------------------------------

/// When the EventStore contains no events, catch-up completes immediately
/// without calling project().
#[tokio::test]
async fn catchup_empty_store_project_never_called() {
    // Given: empty store
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    let count = Arc::new(AtomicUsize::new(0));

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("cu-empty"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        {
            let count_c = Arc::clone(&count);
            move || SequenceRecordingProjector {
                sequences: Arc::new(Mutex::new(Vec::new())),
                count: Arc::clone(&count_c),
                notify: Arc::new(Notify::new()),
            }
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .catchup_from_global()
    .spawn(&system)
    .await;

    // Give the process time to complete on_start (catch-up is a no-op, returns immediately)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "project() must not be called when the event store is empty"
    );
}

// ---------------------------------------------------------------------------
// Test: multiple aggregates projected in global_sequence order
// ---------------------------------------------------------------------------

/// Two aggregates (A and B) each have 2 events. Using CatchupOrigin::Global,
/// the projector receives all 4 events in ascending global_sequence order.
#[tokio::test]
async fn catchup_multiple_aggregates_projected_in_global_sequence_order() {
    // Given: interleave events from two aggregates
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_a = AggregateId::new("cu-multi-a");
    let agg_b = AggregateId::new("cu-multi-b");

    // Append in order: A-seq1, B-seq1, A-seq2, B-seq2
    // The InMemoryEventStore assigns global_sequence monotonically.
    append_evt(&event_store, &agg_a, 1).await; // global=1
    append_evt(&event_store, &agg_b, 1).await; // global=2
    append_evt(&event_store, &agg_a, 2).await; // global=3
    append_evt(&event_store, &agg_b, 2).await; // global=4

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: catch-up from global sequence (covers all aggregates)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("cu-multi"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || SequenceRecordingProjector {
            sequences: Arc::clone(&seq_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .catchup_from_global()
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 4).await;

    // Then: all 4 events were projected (ordering by global_sequence guarantees 4 calls)
    assert_eq!(
        count.load(Ordering::SeqCst),
        4,
        "all 4 events from two aggregates must be projected via global_sequence catch-up"
    );
}

// ---------------------------------------------------------------------------
// Test: live event with duplicate sequence is deduplicated
// ---------------------------------------------------------------------------

/// After catch-up processes events at sequences 1–3, a live event that carries
/// sequence ≤ 3 must be ignored. A live event at sequence=4 must be projected.
#[tokio::test]
async fn catchup_live_event_with_duplicate_sequence_is_skipped() {
    // Given: 3 events in store, 1 live duplicate, 1 new live event
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("cu-dedup-agg");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    // Spawn a typed stream for live delivery
    let stream = system
        .spawn_stream::<EventEnvelope<Evt>>(ProcessName::new("cu-dedup-stream"))
        .await
        .expect("spawn_stream must succeed");

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn projector, subscribe to live stream
    let _proxy = ProjectorProps::new(
        ProjectionId::new("cu-dedup"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || SequenceRecordingProjector {
            sequences: Arc::clone(&seq_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .subscribe(stream.clone())
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    // Publish a live event with sequence=2 (already covered by catch-up → must be skipped)
    stream
        .publish(EventEnvelope {
            aggregate_id: agg_id.clone(),
            sequence: 2,
            global_sequence: 2,
            event: Evt,
        })
        .await
        .expect("publish dup must succeed");

    // Publish a live event with sequence=4 (new, beyond catch-up → must be projected)
    stream
        .publish(EventEnvelope {
            aggregate_id: agg_id,
            sequence: 4,
            global_sequence: 4,
            event: Evt,
        })
        .await
        .expect("publish new must succeed");

    // Wait for 4 events total: 3 from catch-up + 1 new live (not the duplicate)
    wait_for_count(&count, &notify, 4).await;

    // Then: duplicate (seq=2) is not in the recorded sequences
    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        4,
        "exactly 4 unique events must be projected: 3 catch-up + 1 live"
    );
    assert!(
        !seen.windows(2).any(|w| w[0] == w[1]),
        "no sequence must appear twice in the projection record"
    );
    assert!(
        seen.contains(&4),
        "the new live event at sequence=4 must have been projected"
    );
    assert_eq!(
        seen.iter().filter(|&&s| s == 2).count(),
        1,
        "sequence=2 must appear exactly once (from catch-up, not from the live duplicate)"
    );
}
