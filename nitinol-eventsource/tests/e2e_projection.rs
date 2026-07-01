use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event,
    ProjectionContext, Projector, ProjectorProps,
};
use nitinol_persistence::store::{
    CheckpointStore, DeliveryMode, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName, ProjectionId};
use nitinol_runtime::ProcessSystem;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new("e2e.proj"), TypeName::new("Incremented"));
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

struct Increment;

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[derive(Default)]
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

struct CountingProjector {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Incremented> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
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

async fn wait_for_checkpoint(
    store: &Arc<InMemoryCheckpointStore>,
    projection_id: &ProjectionId,
    expected: Option<u64>,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let current = store
                .load(projection_id)
                .await
                .expect("checkpoint load must succeed");
            if current == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for checkpoint to reach {:?}", expected));
}

#[tokio::test]
async fn e2e_aggregate_ask_then_catchup_projection_sees_event() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let id = AggregateId::new("e2e-proj-catchup-agg");

    let agg_proxy = system
        .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&event_store) as Arc<dyn EventStore>)
        .await;
    agg_proxy.ask(Increment).await.expect("ask must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    let _proj_proxy = ProjectorProps::new(
        ProjectionId::new("e2e-proj-catchup"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Incremented>(system.codec::<Incremented>())
    .catchup_from_aggregate(id)
    .spawn(system.process_system())
    .await;

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Incremented) must be called once for the aggregate-persisted event"
    );
}

#[tokio::test]
async fn e2e_live_projection_sees_published_event_envelope() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("e2e-proj-live-agg");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    let _proj_proxy = ProjectorProps::new(
        ProjectionId::new("e2e-proj-live"),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Incremented>(system.codec::<Incremented>())
    .catchup_from_aggregate(agg_id.clone())
    .spawn(system.process_system())
    .await;

    let agg_proxy = system
        .spawn_aggregate::<Counter>(agg_id, Arc::clone(&event_store) as Arc<dyn EventStore>)
        .await;
    agg_proxy.ask(Increment).await.expect("ask must succeed");

    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Incremented) must be called once for the live event"
    );
}

#[tokio::test]
async fn e2e_at_most_once_delivery_advances_checkpoint_before_project() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let id = AggregateId::new("e2e-proj-amo-agg");
    let projection_id = ProjectionId::new("e2e-proj-amo");

    let agg_proxy = system
        .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&event_store) as Arc<dyn EventStore>)
        .await;
    agg_proxy.ask(Increment).await.expect("ask 1");
    agg_proxy.ask(Increment).await.expect("ask 2");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    let _proj_proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Incremented>(system.codec::<Incremented>())
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(id)
    .spawn(system.process_system())
    .await;

    wait_for_count(&count, &notify, 2).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "AtMostOnce: project() must be called for each of the 2 events"
    );

    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(2)).await;
}
