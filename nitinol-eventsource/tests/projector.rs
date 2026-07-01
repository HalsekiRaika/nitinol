use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{codec::Codec, Event, ProjectionContext, Projector, ProjectorProps};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName, Variant, ProjectionId};
use nitinol_runtime::ProcessSystem;

#[derive(Clone)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Counted"));
}

#[derive(Clone)]
struct Labeled;

impl Event for Labeled {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Labeled"));
}

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

fn make_tracking_state() -> (
    Arc<AtomicUsize>, // count (Counted)
    Arc<AtomicUsize>, // label_count (Labeled)
    Arc<Notify>,
    Arc<Mutex<Option<ProjectionId>>>,
    Arc<AtomicUsize>, // last_sequence
) {
    (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Notify::new()),
        Arc::new(Mutex::new(None)),
        Arc::new(AtomicUsize::new(0)),
    )
}

async fn append_counted(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::new(Family::new(""), TypeName::new("Counted")),
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
                event_type: EventType::new(Family::new(""), TypeName::new("Labeled")),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

async fn wait_for_count(counter: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
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

#[tokio::test]
async fn projector_single_event_type_project_called_during_catchup() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-single-agg");

    append_counted(&event_store, &agg_id, 1).await;

    let (count, _lc, notify, _pid, _seq) = make_tracking_state();
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

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

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Counted) must be called exactly once for the one stored event"
    );
}

#[tokio::test]
async fn projector_context_provides_correct_projection_id() {
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

    let seen = last_pid.lock().unwrap().clone();
    assert_eq!(
        seen,
        Some(projection_id),
        "ctx.projection_id() must return the projection_id configured in ProjectorProps"
    );
}

#[tokio::test]
async fn projector_context_provides_current_sequence() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-ctx-seq-agg");

    for seq in 1..=3 {
        append_counted(&event_store, &agg_id, seq).await;
    }

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let last_seq = Arc::new(AtomicUsize::new(0));
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);
    let last_seq_c = Arc::clone(&last_seq);

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

    assert_eq!(
        last_seq.load(Ordering::SeqCst),
        3,
        "ctx.current_sequence() for the last event must equal 3"
    );
}

#[tokio::test]
async fn projector_multiple_event_types_each_receives_correct_event() {
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

    wait_for_count(&count, &notify, 2).await;
    wait_for_count(&label_count, &notify, 1).await;

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

#[tokio::test]
async fn projector_live_event_is_projected_after_catchup() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-live-agg");
    let projection_id = ProjectionId::new("proj-live");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

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
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    append_counted(&event_store, &agg_id, 1).await;

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "Projector<Counted>::project() must be called once for the live event"
    );
}

/// Regression: ProjectorProcess dispatches via type_key(), so a LoadedEvent
/// whose EventType carries variant=Some must reach the handler registered for
/// the type-level EVENT_TYPE (variant=None).
///
/// If process.rs reverted to full `==` instead of `.type_key() ==`, this test
/// would time out because project() would never be called.
#[tokio::test]
async fn projector_variant_some_event_type_dispatches_to_type_level_handler() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("proj-variant-dispatch-agg");

    // Append an event whose EventType has variant=Some but the same family/type_name
    // as Counted::EVENT_TYPE (which has variant=None).
    event_store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: EventType::with_variant(
                    Family::new(""),
                    TypeName::new("Counted"),
                    Variant::new("Created"),
                ),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    let _proxy = ProjectorProps::new(
        ProjectionId::new("proj-variant-dispatch"),
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

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Counted) must be called even when the stored EventType carries variant=Some"
    );
}
