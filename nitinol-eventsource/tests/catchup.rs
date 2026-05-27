use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{codec::Codec, Event, ProjectionContext, Projector, ProjectorProps};
use nitinol_persistence::store::{
    CheckpointStore, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ProcessSystem;

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("Evt");
}

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
        self.sequences.lock().unwrap().push(ctx.current_sequence());
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

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
async fn catchup_all_events_processed_from_empty_checkpoint() {
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

    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![1, 2, 3],
        "all 3 events must be projected in order when there is no checkpoint"
    );
}

#[tokio::test]
async fn catchup_skips_events_before_checkpoint() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("cu-skip-agg");
    let projection_id = ProjectionId::new("cu-skip");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

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
    tokio::time::sleep(Duration::from_millis(300)).await;

    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![3],
        "only the event after the checkpoint (seq=3) must be projected"
    );
}

#[tokio::test]
async fn catchup_empty_store_project_never_called() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    let count = Arc::new(AtomicUsize::new(0));

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

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "project() must not be called when the event store is empty"
    );
}

#[tokio::test]
async fn catchup_multiple_aggregates_projected_in_global_sequence_order() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_a = AggregateId::new("cu-multi-a");
    let agg_b = AggregateId::new("cu-multi-b");

    append_evt(&event_store, &agg_a, 1).await;
    append_evt(&event_store, &agg_b, 1).await;
    append_evt(&event_store, &agg_a, 2).await;
    append_evt(&event_store, &agg_b, 2).await;

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

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

    assert_eq!(
        count.load(Ordering::SeqCst),
        4,
        "all 4 events from two aggregates must be projected via global_sequence catch-up"
    );
}

#[tokio::test]
async fn catchup_then_live_append_is_projected_via_durable_stream() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("cu-then-live-agg");

    for seq in 1..=2 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    let sequences = Arc::new(Mutex::new(Vec::<u64>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seq_c = Arc::clone(&sequences);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    let _proxy = ProjectorProps::new(
        ProjectionId::new("cu-then-live"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || SequenceRecordingProjector {
            sequences: Arc::clone(&seq_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .catchup_from_aggregate(agg_id.clone())
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 2).await;
    append_evt(&event_store, &agg_id, 3).await;

    wait_for_count(&count, &notify, 3).await;

    let seen = sequences.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![1, 2, 3],
        "the live event (seq=3) must be projected after catchup via the same DurableStream"
    );
}
