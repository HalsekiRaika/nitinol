#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Decider, Decision, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.direct"), TypeName::new("OrderPlaced"));
}

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;
    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
}

impl Decider<PlaceOrder> for Order {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced { sku: cmd.sku }]).output(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.direct"),
        TypeName::new("ReservationRequested"),
    );
}

/// Correlation rule of [`RecordingSaga`]: every `OrderPlaced` belongs to the
/// one recording instance each test in this file spawns against its own store.
const RECORDING_SAGA_ID: &str = "direct-store-saga-1";

struct RecordingSaga {
    captured: Arc<Mutex<Vec<SagaId>>>,
    done: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RECORDING_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(ctx.saga_id().clone());
        let notify = Arc::clone(&self.done);
        let effect = SagaEffect::persist(ReservationRequested { sku: event.sku });
        notify.notify_one();
        Ok(effect)
    }
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

async fn wait_for_saga_event_count(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    tokio::time::timeout(timeout, async {
        loop {
            let events = load_saga_events(store, saga_id).await;
            if events.len() >= expected {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} saga events"))
}

#[tokio::test]
async fn aggregate_and_saga_share_one_arc_dyn_event_store() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("direct-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let done_for_producer = Arc::clone(&done);

    let _saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_producer),
                done: Arc::clone(&done_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-direct-1".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    tokio::time::timeout(Duration::from_secs(3), done.notified())
        .await
        .expect("saga must persist within 3 seconds");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let saga_events = load_saga_events(&store, &saga_id).await;

    assert_eq!(
        saga_events.len(),
        1,
        "saga must persist exactly one ReservationRequested event"
    );
    assert_eq!(saga_events[0].sequence, 1);
    assert_eq!(saga_events[0].event_type, ReservationRequested::EVENT_TYPE);
    assert_eq!(
        saga_events[0].stream_key,
        saga_id.as_str(),
        "saga event must be keyed by saga_id (Borrow<str>) — not by an aggregate id"
    );

    let order_events: Vec<_> = store
        .load(LoadQuery::by_stream("direct-order"))
        .await
        .expect("load order stream must succeed")
        .try_collect()
        .await
        .expect("collect order events must succeed");
    assert_eq!(
        order_events.len(),
        1,
        "aggregate stream must coexist with saga stream in the same store"
    );

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].as_str(), RECORDING_SAGA_ID);
}

/// Regression test: dropping all `SagaProxy` handles must NOT stop the
/// upstream `DurableStream` subscription.  The subscription lifetime is owned by `SagaProcess` itself,
/// so the process continues receiving and persisting events after every handle
/// has been released.
#[tokio::test]
async fn saga_proxy_drop_does_not_stop_upstream_subscription() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("drop-proxy-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let done_for_producer = Arc::clone(&done);

    let saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_producer),
                done: Arc::clone(&done_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-drop-1".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");
    wait_for_saga_event_count(&store, &saga_id, 1, Duration::from_secs(3)).await;

    drop(saga_proxy);

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-drop-2".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    let saga_events = wait_for_saga_event_count(&store, &saga_id, 2, Duration::from_secs(3)).await;

    assert_eq!(
        saga_events.len(),
        2,
        "SagaProxy drop must not stop the upstream subscription; \
         SagaProcess must continue persisting events after the handle is released"
    );
    assert_eq!(saga_events[0].sequence, 1);
    assert_eq!(saga_events[1].sequence, 2);
}

#[tokio::test]
async fn saga_replays_its_own_stream_via_direct_store_on_respawn() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("replay-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_first = Arc::clone(&captured);
    let done_first = Arc::clone(&done);

    let saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_first),
                done: Arc::clone(&done_first),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-1".into(),
        })
        .await
        .expect("ask must succeed");
    wait_for_saga_event_count(&store, &saga_id, 1, Duration::from_secs(3)).await;

    // Explicitly stop the saga process so the upstream DurableStream poller is
    // also torn down.  Dropping the proxy handle alone is not sufficient —
    // the process is owned by the runtime, not by the handle.
    saga_proxy.stop().await.expect("stop must succeed");
    drop(saga_proxy);

    let done2 = Arc::new(Notify::new());
    let captured2 = Arc::clone(&captured);
    let done2_for_producer = Arc::clone(&done2);

    let _saga_proxy2 =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured2),
                done: Arc::clone(&done2_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-2".into(),
        })
        .await
        .expect("ask must succeed");

    let saga_events = wait_for_saga_event_count(&store, &saga_id, 3, Duration::from_secs(3)).await;

    assert_eq!(
        saga_events.len(),
        3,
        "saga stream must contain three events after respawn (1 pre-drop, \
         2 post-respawn from at-least-once catchup + new live event)"
    );
    assert_eq!(saga_events[0].sequence, 1);
    assert_eq!(saga_events[1].sequence, 2);
    assert_eq!(
        saga_events[2].sequence, 3,
        "highest saga sequence must be 3, proving on_start restored \
         state.sequence to 1 from the direct store"
    );
}
