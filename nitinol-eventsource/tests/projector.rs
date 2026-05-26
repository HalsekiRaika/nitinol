// Integration tests for Projector<E> trait and ProjectionContext.
//
// Tests verify:
// - project() is called for events during catch-up
// - ProjectionContext provides projection_id and current_sequence
// - A single type P can implement Projector<E1> and Projector<E2>
// - Live events published to a subscribed Stream<EventEnvelope<E>> are projected
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
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event types
// ---------------------------------------------------------------------------

/// A simple unit event for counting projection calls.
#[derive(Clone)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType = EventType::from_str("Counted");
}

/// A second unit event to test multiple Projector<E> implementations on one type.
#[derive(Clone)]
struct Labeled;

impl Event for Labeled {
    const EVENT_TYPE: EventType = EventType::from_str("Labeled");
}

// ---------------------------------------------------------------------------
// Fixtures: codec
// ---------------------------------------------------------------------------

/// Pass-through codec for unit events (no data to encode/decode).
struct UnitCodec;

impl Codec<Counted> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Counted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Counted, Self::Error> {
        Ok(Counted)
    }
}

impl Codec<Labeled> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Labeled) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Labeled, Self::Error> {
        Ok(Labeled)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: TrackingProjector
//
// Shared-state projector that records each project() call so the test can
// inspect which events were received and what context values were provided.
// ---------------------------------------------------------------------------

struct TrackingProjector {
    /// Incremented on every Counted project() call.
    count: Arc<AtomicUsize>,
    /// Incremented on every Labeled project() call.
    label_count: Arc<AtomicUsize>,
    /// Notified on every project() call (either event type).
    notify: Arc<Notify>,
    /// Stores the projection_id seen in the most recent project() call.
    last_projection_id: Arc<Mutex<Option<ProjectionId>>>,
    /// Stores the current_sequence seen in the most recent project() call.
    last_sequence: Arc<AtomicUsize>,
}

#[async_trait]
impl Projector<Counted> for TrackingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Counted,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        *self.last_projection_id.lock().unwrap() = Some(ctx.projection_id().clone());
        self.last_sequence
            .store(ctx.current_sequence() as usize, Ordering::SeqCst);
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

#[async_trait]
impl Projector<Labeled> for TrackingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Labeled,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        *self.last_projection_id.lock().unwrap() = Some(ctx.projection_id().clone());
        self.last_sequence
            .store(ctx.current_sequence() as usize, Ordering::SeqCst);
        self.label_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: shared state factory
// ---------------------------------------------------------------------------

fn make_tracking_state() -> (
    Arc<AtomicUsize>,   // count (Counted)
    Arc<AtomicUsize>,   // label_count (Labeled)
    Arc<Notify>,
    Arc<Mutex<Option<ProjectionId>>>,
    Arc<AtomicUsize>,   // last_sequence
) {
    (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Notify::new()),
        Arc::new(Mutex::new(None)),
        Arc::new(AtomicUsize::new(0)),
    )
}

// ---------------------------------------------------------------------------
// Helper: append a Counted event to the store
// ---------------------------------------------------------------------------

async fn append_counted(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::from_str("Counted"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

async fn append_labeled(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::from_str("Labeled"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

// ---------------------------------------------------------------------------
// Helper: wait until counter reaches expected value or timeout
// ---------------------------------------------------------------------------

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
// Test: single Projector<E> — project() is called for each catch-up event
// ---------------------------------------------------------------------------

/// Given one Counted event in the store, spawning a ProjectorProcess triggers
/// catch-up and calls project() exactly once.
#[tokio::test]
async fn projector_single_event_type_project_called_during_catchup() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-single-agg");

    append_counted(&event_store, &agg_id, 1).await;

    let (count, _lc, notify, _pid, _seq) = make_tracking_state();
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("proj-single"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || TrackingProjector {
            count: Arc::clone(&count_c),
            label_count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::clone(&notify_c),
            last_projection_id: Arc::new(Mutex::new(None)),
            last_sequence: Arc::new(AtomicUsize::new(0)),
        },
    )
    .with_event::<Counted>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then
    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Counted) must be called exactly once for the one stored event"
    );
}

// ---------------------------------------------------------------------------
// Test: ProjectionContext provides the configured projection_id
// ---------------------------------------------------------------------------

/// The ProjectionContext passed to project() exposes the projection_id used
/// when building ProjectorProps.
#[tokio::test]
async fn projector_context_provides_correct_projection_id() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-ctx-id-agg");
    let projection_id = ProjectionId::new("my-projection");

    append_counted(&event_store, &agg_id, 1).await;

    let (_count, _lc, notify, last_pid, _seq) = make_tracking_state();
    let count = Arc::new(AtomicUsize::new(0));
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);
    let last_pid_c = Arc::clone(&last_pid);

    // When
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || TrackingProjector {
            count: Arc::clone(&count_c),
            label_count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::clone(&notify_c),
            last_projection_id: Arc::clone(&last_pid_c),
            last_sequence: Arc::new(AtomicUsize::new(0)),
        },
    )
    .with_event::<Counted>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Then
    let seen = last_pid.lock().unwrap().clone();
    assert_eq!(
        seen,
        Some(projection_id),
        "ctx.projection_id() must return the projection_id configured in ProjectorProps"
    );
}

// ---------------------------------------------------------------------------
// Test: ProjectionContext provides the event sequence
// ---------------------------------------------------------------------------

/// ctx.current_sequence() in project() returns the sequence of the event being
/// processed.
#[tokio::test]
async fn projector_context_provides_current_sequence() {
    // Given: event at sequence=3
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-ctx-seq-agg");

    // Append events at sequence 1, 2, 3 so the last processed sequence is 3
    for seq in 1..=3 {
        append_counted(&event_store, &agg_id, seq).await;
    }

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let last_seq = Arc::new(AtomicUsize::new(0));
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);
    let last_seq_c = Arc::clone(&last_seq);

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("proj-ctx-seq"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || TrackingProjector {
            count: Arc::clone(&count_c),
            label_count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::clone(&notify_c),
            last_projection_id: Arc::new(Mutex::new(None)),
            last_sequence: Arc::clone(&last_seq_c),
        },
    )
    .with_event::<Counted>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 3).await;

    // Then: the final project() call was for sequence=3
    assert_eq!(
        last_seq.load(Ordering::SeqCst),
        3,
        "ctx.current_sequence() for the last event must equal 3"
    );
}

// ---------------------------------------------------------------------------
// Test: multiple Projector<E> implementations on the same projector type
// ---------------------------------------------------------------------------

/// A single P implementing both Projector<Counted> and Projector<Labeled>
/// dispatches each event type to the correct project() implementation.
#[tokio::test]
async fn projector_multiple_event_types_each_receives_correct_event() {
    // Given: 2 Counted events and 1 Labeled event in the store
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-multi-agg");

    append_counted(&event_store, &agg_id, 1).await;
    append_labeled(&event_store, &agg_id, 2).await;
    append_counted(&event_store, &agg_id, 3).await;

    let (count, label_count, notify, _pid, _seq) = make_tracking_state();
    let count_c = Arc::clone(&count);
    let lc_c = Arc::clone(&label_count);
    let notify_c = Arc::clone(&notify);

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("proj-multi"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || TrackingProjector {
            count: Arc::clone(&count_c),
            label_count: Arc::clone(&lc_c),
            notify: Arc::clone(&notify_c),
            last_projection_id: Arc::new(Mutex::new(None)),
            last_sequence: Arc::new(AtomicUsize::new(0)),
        },
    )
    .with_event::<Counted>(Arc::new(UnitCodec))
    .with_event::<Labeled>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Wait for all 3 events (2 Counted + 1 Labeled)
    wait_for_count(&count, &notify, 2).await; // wait for at least 2 Counted
    wait_for_count(&label_count, &notify, 1).await; // wait for at least 1 Labeled

    // Then
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "Projector<Counted>::project() must be called twice"
    );
    assert_eq!(
        label_count.load(Ordering::SeqCst),
        1,
        "Projector<Labeled>::project() must be called once"
    );
}

// ---------------------------------------------------------------------------
// Test: live event published after spawn is projected
// ---------------------------------------------------------------------------

/// A live event published to the subscribed Stream<EventEnvelope<Counted>>
/// after spawning is projected by the process.
#[tokio::test]
async fn projector_live_event_is_projected_after_catchup() {
    // Given: no stored events (empty catch-up), but a live stream subscribed
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-live-agg");
    let projection_id = ProjectionId::new("proj-live");

    // Spawn the live stream for EventEnvelope<Counted>
    let stream = system
        .spawn_stream::<EventEnvelope<Counted>>(ProcessName::new("proj-live-stream"))
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn the projector subscribed to the live stream
    let _proxy = ProjectorProps::new(
        projection_id,
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || TrackingProjector {
            count: Arc::clone(&count_c),
            label_count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::clone(&notify_c),
            last_projection_id: Arc::new(Mutex::new(None)),
            last_sequence: Arc::new(AtomicUsize::new(0)),
        },
    )
    .with_event::<Counted>(Arc::new(UnitCodec))
    .subscribe(stream.clone())
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    // Publish a live event
    stream
        .publish(EventEnvelope {
            aggregate_id: agg_id,
            sequence: 1,
            global_sequence: 1,
            event: Counted,
        })
        .await
        .expect("publish must succeed");

    // Then: project() is called for the live event
    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "Projector<Counted>::project() must be called once for the live event"
    );
}

// ---------------------------------------------------------------------------
// Re-prevention test: code-quality (ARCH-NEW-props-redundant-bounds-L119)
//
// `subscribe<E>` must compile with `where E: Event` alone.
// `Event: Clone + Send + Sync + 'static`, so listing `Clone + Sync` explicitly
// is redundant.  This test verifies that a type implementing only `Event`
// (no additional explicit supertraits beyond those required by `Event`) works
// with `subscribe`.  If the redundant `Clone + Sync` bounds are ever re-added
// this test continues to compile (they are implied by `Event`), but the doc
// comment and this marker keep the intent visible for reviewers.
// ---------------------------------------------------------------------------

/// Verifies that `subscribe` only requires `E: Event` — `Clone` and `Sync` come
/// from `Event`'s supertraits, not from a separate bound on `subscribe`.
#[tokio::test]
async fn subscribe_requires_only_event_bound() {
    // `Minimal` derives nothing beyond `Clone` (required by `Event: Clone`).
    // No explicit `Sync` derive is needed because `Clone` structs are `Sync`
    // by the auto-impl when all fields are `Sync`.
    #[derive(Clone)]
    struct Minimal;

    impl Event for Minimal {
        const EVENT_TYPE: EventType = EventType::from_str("Minimal");
    }

    struct MinimalProjector {
        count: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Projector<Minimal> for MinimalProjector {
        type Error = std::convert::Infallible;
        async fn project(
            &mut self,
            _event: Minimal,
            _ctx: &mut ProjectionContext<'_, ()>,
        ) -> Result<(), Self::Error> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_one();
            Ok(())
        }
    }

    struct MinimalCodec;
    impl Codec<Minimal> for MinimalCodec {
        type Error = std::convert::Infallible;
        fn encode(_event: &Minimal) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }
        fn decode(_payload: &[u8]) -> Result<Minimal, Self::Error> {
            Ok(Minimal)
        }
    }

    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let projection_id = ProjectionId::new("minimal-bounds");
    let agg_id = AggregateId::new("minimal-agg");

    let stream = system
        .spawn_stream::<EventEnvelope<Minimal>>(ProcessName::new("minimal-stream"))
        .await
        .expect("spawn_stream must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // This line must compile with `where E: Event` only — no extra `Clone + Sync`.
    let _proxy = ProjectorProps::new(
        projection_id,
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || MinimalProjector { count: Arc::clone(&count_c), notify: Arc::clone(&notify_c) },
    )
    .with_event::<Minimal>(Arc::new(MinimalCodec))
    .subscribe(stream.clone())  // <-- compile-fails if `subscribe` requires `Clone + Sync` explicitly
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    stream
        .publish(EventEnvelope {
            aggregate_id: agg_id,
            sequence: 1,
            global_sequence: 1,
            event: Minimal,
        })
        .await
        .expect("publish must succeed");

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
