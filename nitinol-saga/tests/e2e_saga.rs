//! End-to-end test: an upstream aggregate's event drives the saga to issue a
//! command against a downstream aggregate.  Verifies that the new ADT's
//! `Persist`-with-tells branch correctly:
//!   1. Persists the user event AND a TellRequested outbox marker atomically
//!   2. Dispatches the typed command (with `C: Clone`) to the target
//!   3. Appends a TellAcked outbox marker once the dispatch succeeds

#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    Receive as EvtReceive, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.saga"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.saga"), TypeName::new("Reserved"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.saga"),
        TypeName::new("ReservationRequested"),
    );
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
/// `SagaEffect::tell` keeps the command across retry attempts and serializes
/// it as crash-restart payload into the `TellRequested` outbox marker.
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

#[derive(Debug, Clone)]
struct CapturedContext {
    saga_id: SagaId,
    sequence: u64,
}

/// Correlation rule of [`ReservationSaga`]: the one reservation process the
/// end-to-end test spawns owns every order it sees.
const RESERVATION_SAGA_ID: &str = "saga-e2e-reservation-1";

/// Correlation rule of [`MatchOnlyReservationSaga`]: only orders whose SKU
/// carries the `MATCH-` prefix belong to it.
const MATCH_ONLY_SAGA_ID: &str = "saga-route-match";

/// The reaction both sagas below perform.  Only their correlation rules differ,
/// and correlation is now a property of the type, so the shared behaviour lives
/// here rather than being written twice.
struct ReservationWork {
    inventory: AggregateProxy<Inventory>,
    captured: Arc<Mutex<Vec<CapturedContext>>>,
    handle_count: Arc<Mutex<u64>>,
}

impl ReservationWork {
    fn reserve(
        &mut self,
        event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> SagaEffect<ReservationRequested> {
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(CapturedContext {
                saga_id: ctx.saga_id().clone(),
                sequence: ctx.sequence(),
            });
        *self.handle_count.lock().expect(
            "handle_count mutex is never poisoned: no holder panics while the guard is alive",
        ) += 1;

        let persist_own_event = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell_inventory = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        persist_own_event.combine(tell_inventory)
    }
}

struct ReservationSaga(ReservationWork);

#[async_trait]
impl Saga for ReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESERVATION_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(self.0.reserve(event, ctx))
    }
}

/// A saga whose correlation declines part of the upstream stream.  It is a
/// distinct type from [`ReservationSaga`] because correlation belongs to the
/// type: folding this rule into `ReservationSaga` would starve the end-to-end
/// test, whose SKUs carry no `MATCH-` prefix.
struct MatchOnlyReservationSaga(ReservationWork);

#[async_trait]
impl Saga for MatchOnlyReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        event
            .sku
            .starts_with("MATCH-")
            .then(|| SagaId::new(MATCH_ONLY_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(self.0.reserve(event, ctx))
    }
}

fn count_user_events(events: &[LoadedEvent], expected: EventType) -> usize {
    events.iter().filter(|e| e.event_type == expected).count()
}

fn count_outbox_events(events: &[LoadedEvent], pred: impl Fn(&OutboxKind) -> bool) -> usize {
    events
        .iter()
        .filter(|e| outbox_kind_of(e).as_ref().is_some_and(&pred))
        .count()
}

async fn wait_until_outbox_acked(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_outbox_events(&events, |k| matches!(k, OutboxKind::TellAcked(_))) >= 1 {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for TellAcked outbox event in saga stream (event_types: {:?})",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn aggregate_event_drives_saga_to_command_target_aggregate() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-e2e-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&order_store))
        .await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(AggregateId::new("saga-e2e-inventory"), inventory_store)
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_store_for_assert = Arc::clone(&saga_store);

    let saga_id = SagaId::new(RESERVATION_SAGA_ID);
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let handle_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let inventory_for_producer = inventory_proxy.clone();
    let captured_for_producer = Arc::clone(&captured);
    let handle_count_for_producer = Arc::clone(&handle_count);

    let _saga_proxy = SagaProps::<ReservationSaga>::new(saga_id.clone(), saga_store, move || {
        ReservationSaga(ReservationWork {
            inventory: inventory_for_producer.clone(),
            captured: Arc::clone(&captured_for_producer),
            handle_count: Arc::clone(&handle_count_for_producer),
        })
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

    // Poll until Inventory has processed the Reserve command.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let count = loop {
        let c = inventory_proxy
            .exec(GetReservedCount)
            .await
            .expect("exec(GetReservedCount) must succeed");
        if c >= 1 {
            break c;
        }
        if std::time::Instant::now() >= deadline {
            panic!("saga must drive a Reserve into Inventory within 3 seconds");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        count, 1,
        "Inventory must have received exactly one Reserve command — \
         a single-attempt success must not double-dispatch even though the executor performs retries"
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
        "saga-e2e-reservation-1",
        "SagaContext::saga_id must reflect the spawned saga instance"
    );
    assert_eq!(
        captured[0].sequence, 0,
        "fresh saga has sequence 0 before its first SagaEffect::Persist"
    );

    let saga_events = wait_until_outbox_acked(&saga_store_for_assert, &saga_id).await;

    assert_eq!(
        count_user_events(&saga_events, ReservationRequested::EVENT_TYPE),
        1,
        "saga must persist exactly one ReservationRequested user event"
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

    // The user event must be appended at sequence 1.  The TellRequested marker
    // must share the same atomic append batch, so it lands at sequence 2.
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
        "the user event must be appended at sequence 1"
    );
    assert_eq!(
        requested.sequence, 2,
        "TellRequested must share the same atomic batch as the user event and land at sequence 2"
    );
}

#[tokio::test]
async fn saga_skips_events_that_correlate_to_no_instance() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(AggregateId::new("saga-route-inventory"), inventory_store)
        .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let publisher_id = AggregateId::new("publisher-A");
    append_order_placed(&upstream_store, &publisher_id, 1, "SKIP-001").await;
    append_order_placed(&upstream_store, &publisher_id, 2, "MATCH-001").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let handle_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let inventory_for_producer = inventory_proxy.clone();
    let captured_for_producer = Arc::clone(&captured);
    let handle_count_for_producer = Arc::clone(&handle_count);

    let _saga_proxy = SagaProps::<MatchOnlyReservationSaga>::new(
        SagaId::new(MATCH_ONLY_SAGA_ID),
        saga_store,
        move || {
            MatchOnlyReservationSaga(ReservationWork {
                inventory: inventory_for_producer.clone(),
                captured: Arc::clone(&captured_for_producer),
                handle_count: Arc::clone(&handle_count_for_producer),
            })
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: publisher_id.as_str().to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    // Poll until Inventory has processed the Reserve command for the MATCH event.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let c = inventory_proxy
            .exec(GetReservedCount)
            .await
            .expect("exec(GetReservedCount) must succeed");
        if c >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("the matched event must reach Inventory within 3 seconds");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    let final_count = *handle_count
        .lock()
        .expect("handle_count mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        final_count, 1,
        "Saga::handle must run exactly once — only the event this saga \
         correlates to must reach it"
    );

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        captured.len(),
        1,
        "exactly one event must be captured by handle()"
    );
}

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
    sequence: u64,
    sku: &str,
) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderPlaced must succeed");
}

async fn load_saga_events(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
) -> Vec<nitinol_persistence::LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("saga event store load must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}
