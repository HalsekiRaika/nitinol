mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    EventEnvelope, EventPersistor, EventPersistorProxy, Receive as EvtReceive,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, LoadQuery};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::from_str("e2e.saga.OrderPlaced");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType = EventType::from_str("e2e.saga.Reserved");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::from_str("e2e.saga.ReservationRequested");
}

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;

    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
    stream: nitinol_runtime::process::ProcessProxy<
        nitinol_runtime::process::Stream<EventEnvelope<OrderPlaced>>,
    >,
}

#[async_trait]
impl Decider<PlaceOrder> for Order {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PlaceOrder,
        ctx: &mut Context,
    ) -> Result<Effect<OrderPlaced>, Self::Rejection> {
        let envelope = EventEnvelope {
            aggregate_id: ctx.aggregate_id().clone(),
            sequence: ctx.sequence() + 1,
            global_sequence: 0,
            event: OrderPlaced {
                sku: cmd.sku.clone(),
            },
        };
        Ok(Effect::persist(OrderPlaced {
            sku: cmd.sku,
        })
        .combine(Effect::publish(cmd.stream, envelope)))
    }
}

#[derive(Default)]
struct Inventory {
    reserved_count: u64,
}

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {
        self.reserved_count += 1;
    }
}

struct Reserve {
    sku: String,
    done_notify: Arc<Notify>,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        let done = cmd.done_notify.clone();
        let effect = Effect::persist(Reserved { sku: cmd.sku });
        done.notify_one();
        Ok(effect)
    }
}

struct GetReservedCount;

#[async_trait]
impl EvtReceive<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(
        &self,
        _msg: GetReservedCount,
        _ctx: &mut Context,
    ) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}

#[derive(Debug, Clone)]
struct CapturedContext {
    saga_id: SagaId,
    sequence: u64,
}

struct ReservationSaga {
    inventory: AggregateProxy<Inventory>,
    done_notify: Arc<Notify>,
    captured: Arc<Mutex<Vec<CapturedContext>>>,
    handle_count: Arc<Mutex<u64>>,
}

#[async_trait]
impl Saga for ReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured.lock().unwrap().push(CapturedContext {
            saga_id: ctx.saga_id().clone(),
            sequence: ctx.sequence(),
        });
        *self.handle_count.lock().unwrap() += 1;

        let persist_own_event =
            SagaEffect::persist(ReservationRequested { sku: event.sku.clone() });
        let tell_inventory = SagaEffect::tell(
            self.inventory.clone(),
            Reserve {
                sku: event.sku,
                done_notify: Arc::clone(&self.done_notify),
            },
        );
        Ok(persist_own_event.combine(tell_inventory))
    }
}

#[tokio::test]
async fn aggregate_event_drives_saga_to_command_target_aggregate() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_event_ref =
        EventPersistor::spawn(system.process_system(), Arc::clone(&order_store)).await;
    let order_proxy = system
        .spawn_aggregate::<Order>(AggregateId::new("saga-e2e-order"), order_event_ref)
        .await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_event_ref =
        EventPersistor::spawn(system.process_system(), Arc::clone(&inventory_store)).await;
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(
            AggregateId::new("saga-e2e-inventory"),
            inventory_event_ref,
        )
        .await;

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<OrderPlaced>>(ProcessName::new("saga-e2e-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_event_ref =
        EventPersistor::spawn(system.process_system(), Arc::clone(&saga_store)).await;
    let saga_event_ref_for_assert = saga_event_ref.clone();

    let saga_id = SagaId::new("saga-e2e-reservation-1");
    let done = Arc::new(Notify::new());
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let handle_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let route_target = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(route_target.clone()) };

    let inventory_for_producer = inventory_proxy.clone();
    let done_for_producer = Arc::clone(&done);
    let captured_for_producer = Arc::clone(&captured);
    let handle_count_for_producer = Arc::clone(&handle_count);

    let _saga_proxy = SagaProps::<ReservationSaga>::new(
        saga_id.clone(),
        saga_event_ref,
        move || ReservationSaga {
            inventory: inventory_for_producer.clone(),
            done_notify: Arc::clone(&done_for_producer),
            captured: Arc::clone(&captured_for_producer),
            handle_count: Arc::clone(&handle_count_for_producer),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), route_fn)
    .spawn(system.process_system())
    .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-001".into(),
            stream: stream.clone(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    tokio::time::timeout(Duration::from_millis(500), done.notified())
        .await
        .expect("saga must drive a Reserve into Inventory within 500 ms");

    let count = inventory_proxy
        .exec(GetReservedCount)
        .await
        .expect("exec(GetReservedCount) must succeed");
    assert_eq!(
        count, 1,
        "Inventory must have received exactly one Reserve command"
    );

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "Saga::handle must be invoked exactly once"
    );
    assert_eq!(
        captured[0].saga_id.as_str(),
        "saga-e2e-reservation-1",
        "SagaContext::saga_id must reflect the spawned saga instance"
    );
    assert_eq!(
        captured[0].sequence, 0,
        "fresh saga has sequence 0 before its first SagaEffect::Persist"
    );

    let saga_events = load_saga_events(&saga_event_ref_for_assert, &saga_id).await;
    assert_eq!(
        saga_events.len(),
        1,
        "saga must persist exactly one ReservationRequested event"
    );
    assert_eq!(
        saga_events[0].event_type,
        ReservationRequested::EVENT_TYPE,
        "saga's persisted event must be ReservationRequested"
    );
    assert_eq!(
        saga_events[0].sequence, 1,
        "saga's first persisted event must be at sequence 1"
    );
}

#[tokio::test]
async fn saga_skips_events_not_routed_to_its_instance() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_event_ref =
        EventPersistor::spawn(system.process_system(), Arc::clone(&inventory_store)).await;
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(
            AggregateId::new("saga-route-inventory"),
            inventory_event_ref,
        )
        .await;

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<OrderPlaced>>(ProcessName::new("saga-route-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_event_ref =
        EventPersistor::spawn(system.process_system(), Arc::clone(&saga_store)).await;

    let matched_id = SagaId::new("saga-route-match");
    let done = Arc::new(Notify::new());
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let handle_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let matched_for_route = matched_id.clone();
    let route_fn = move |event: &OrderPlaced| -> Option<SagaId> {
        if event.sku.starts_with("MATCH-") {
            Some(matched_for_route.clone())
        } else {
            None
        }
    };

    let inventory_for_producer = inventory_proxy.clone();
    let done_for_producer = Arc::clone(&done);
    let captured_for_producer = Arc::clone(&captured);
    let handle_count_for_producer = Arc::clone(&handle_count);

    let _saga_proxy = SagaProps::<ReservationSaga>::new(
        matched_id.clone(),
        saga_event_ref,
        move || ReservationSaga {
            inventory: inventory_for_producer.clone(),
            done_notify: Arc::clone(&done_for_producer),
            captured: Arc::clone(&captured_for_producer),
            handle_count: Arc::clone(&handle_count_for_producer),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), route_fn)
    .spawn(system.process_system())
    .await;

    stream
        .publish(EventEnvelope {
            aggregate_id: AggregateId::new("publisher-A"),
            sequence: 1,
            global_sequence: 1,
            event: OrderPlaced {
                sku: "SKIP-001".into(),
            },
        })
        .await
        .expect("publish (skip) must succeed");

    stream
        .publish(EventEnvelope {
            aggregate_id: AggregateId::new("publisher-A"),
            sequence: 2,
            global_sequence: 2,
            event: OrderPlaced {
                sku: "MATCH-001".into(),
            },
        })
        .await
        .expect("publish (match) must succeed");

    tokio::time::timeout(Duration::from_millis(500), done.notified())
        .await
        .expect("the matched event must reach Inventory within 500 ms");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let final_count = *handle_count.lock().unwrap();
    assert_eq!(
        final_count, 1,
        "Saga::handle must run exactly once — only the routed event must reach it"
    );

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "exactly one event must be captured by handle()"
    );
}

async fn load_saga_events(
    event_ref: &EventPersistorProxy,
    saga_id: &SagaId,
) -> Vec<nitinol_persistence::LoadedEvent> {
    event_ref
        .load(LoadQuery::by_aggregate(saga_id.clone()))
        .await
        .expect("saga event store load must succeed")
}
