//! The resident single-instance saga is expressible as a manager whose
//! correlation is constant.
//!
//! This is the migration path check.  A `Saga::correlate` that always answers
//! the same [`SagaId`] reduces the manager to exactly one instance, and the
//! observable result must match what `SagaProps` produces today for the same
//! scenario — the assertions mirror
//! `nitinol-saga/tests/e2e_saga.rs::aggregate_event_drives_saga_to_command_target_aggregate`
//! so a divergence in outbox ordering, sequences or configuration reach shows up
//! here.

#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    Receive as EvtReceive, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

/// The single instance every upstream event correlates to.  Constant
/// correlation is what makes the manager degenerate to one resident saga.
const RESIDENT_SAGA_ID: &str = "mgr-degenerate-reservation-1";

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.degenerate"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.degenerate"), TypeName::new("Reserved"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("mgr.degenerate"),
        TypeName::new("ReservationRequested"),
    );
}

// Aggregates

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;

    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
}

#[async_trait]
impl Decider<PlaceOrder> for Order {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PlaceOrder,
        _ctx: &mut Context,
    ) -> Result<Effect<OrderPlaced>, Self::Rejection> {
        Ok(Effect::persist(OrderPlaced { sku: cmd.sku }))
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

/// `Reserve` derives `Clone` + `Serialize` + `Deserialize` because
/// `SagaEffect::tell` keeps the command across retry attempts and serializes it
/// as the crash-restart payload of the `TellRequested` outbox marker.
#[derive(Clone, Serialize, Deserialize)]
struct Reserve {
    sku: String,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        Ok(Effect::persist(Reserved { sku: cmd.sku }))
    }
}

struct GetReservedCount;

#[async_trait]
impl EvtReceive<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetReservedCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}

// Saga under test

#[derive(Debug, Clone)]
struct CapturedContext {
    saga_id: SagaId,
    sequence: u64,
}

struct ReservationSaga {
    inventory: AggregateProxy<Inventory>,
    captured: Arc<Mutex<Vec<CapturedContext>>>,
}

#[async_trait]
impl Saga for ReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
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
            .push(CapturedContext {
                saga_id: ctx.saga_id().clone(),
                sequence: ctx.sequence(),
            });

        let persist_own_event = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell_inventory = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        Ok(persist_own_event.combine(tell_inventory))
    }
}

// Helpers

fn count_user_events(events: &[LoadedEvent], expected: EventType) -> usize {
    events.iter().filter(|e| e.event_type == expected).count()
}

fn count_outbox_events(events: &[LoadedEvent], pred: impl Fn(&OutboxKind) -> bool) -> usize {
    events
        .iter()
        .filter(|e| outbox_kind_of(e).as_ref().is_some_and(&pred))
        .count()
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("saga event store load must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

async fn wait_until_outbox_acked(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_outbox_events(&events, |k| matches!(k, OutboxKind::TellAcked(_))) >= 1 {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for TellAcked outbox event in saga stream (event_types: {:?})",
            events
                .iter()
                .map(|e| e.event_type.to_string())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Test

#[tokio::test]
async fn single_constant_correlation_reproduces_the_resident_saga() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("mgr-degenerate-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&order_store))
        .await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(
            AggregateId::new("mgr-degenerate-inventory"),
            inventory_store,
        )
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));

    let inventory_for_producer = inventory_proxy.clone();
    let captured_for_producer = Arc::clone(&captured);

    let _manager_proxy =
        SagaManagerProps::<ReservationSaga>::new(Arc::clone(&saga_store), move || {
            ReservationSaga {
                inventory: inventory_for_producer.clone(),
                captured: Arc::clone(&captured_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&order_store),
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
            sku: "SKU-001".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let reserved_count = loop {
        let count = inventory_proxy
            .exec(GetReservedCount)
            .await
            .expect("exec(GetReservedCount) must succeed");
        if count >= 1 {
            break count;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the manager's single instance must drive a Reserve into Inventory"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        reserved_count, 1,
        "Inventory must have received exactly one Reserve command, as it does \
         under the resident SagaProps wiring"
    );

    let captured = {
        let guard = captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
        guard.clone()
    };
    assert_eq!(
        captured.len(),
        1,
        "Saga::handle must be invoked exactly once"
    );
    assert_eq!(
        captured[0].saga_id.as_str(),
        RESIDENT_SAGA_ID,
        "SagaContext::saga_id must be the id Saga::correlate returned, so the \
         manager's instance is indistinguishable from the resident one"
    );
    assert_eq!(
        captured[0].sequence, 0,
        "a fresh instance has sequence 0 before its first SagaEffect::Persist"
    );

    let saga_events = wait_until_outbox_acked(&saga_store, &saga_id).await;

    assert_eq!(
        count_user_events(&saga_events, ReservationRequested::EVENT_TYPE),
        1,
        "the instance must persist exactly one ReservationRequested user event"
    );
    assert_eq!(
        count_outbox_events(&saga_events, |k| matches!(k, OutboxKind::TellRequested(_))),
        1,
        "Persist with one tell must append exactly one TellRequested outbox event"
    );
    assert_eq!(
        count_outbox_events(&saga_events, |k| matches!(k, OutboxKind::TellAcked(_))),
        1,
        "a successful tell dispatch must result in exactly one TellAcked outbox event"
    );
    assert_eq!(
        count_outbox_events(&saga_events, |k| matches!(k, OutboxKind::TellFailed(_))),
        0,
        "a successful tell must not produce a TellFailed event"
    );

    let user_event = saga_events
        .iter()
        .find(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .expect("ReservationRequested user event must exist in saga stream");
    let requested = saga_events
        .iter()
        .find(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_))))
        .expect("TellRequested outbox event must exist in saga stream");
    assert_eq!(
        user_event.sequence, 1,
        "the user event must be appended at sequence 1, as under SagaProps"
    );
    assert_eq!(
        requested.sequence, 2,
        "TellRequested must share the same atomic batch as the user event and \
         land at sequence 2, as under SagaProps"
    );
}
