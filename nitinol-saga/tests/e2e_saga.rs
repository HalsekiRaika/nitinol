//! End-to-end: an upstream aggregate's event drives a saga, and the decision
//! the saga persisted becomes a command at a downstream aggregate.
//!
//! ```gherkin
//!   Scenario: an order reaches inventory through a saga
//!     Given an Order aggregate, a reservation saga observing it, and a
//!       delivery host over the saga's own store
//!     When an order is placed
//!     Then the saga persists exactly one decision and settles the input it
//!       was decided from, and nothing else
//!     And the command the saga's mapping names reaches the Inventory aggregate
//!
//!   Scenario: an upstream event belonging to nobody
//!     Given a saga whose correlation declines part of the upstream stream
//!     When a declined record and a matching record are appended upstream
//!     Then only the matching one ever reaches `Saga::handle`
//! ```
//!
//! The chain this file walks is deliberately in two halves, and the point of an
//! end-to-end test is that they meet.  `Saga::handle` decides and persists;
//! `Saga::dispatch` says, as a pure function of the decision, what that decision
//! means for everybody else; and a delivery host is the thing that carries it
//! out.  No part of the saga reaches the Inventory aggregate itself, so the
//! first scenario below spawns a host over the saga's own store and then asks
//! Inventory what it was asked to do — if the mapping, the host and the
//! receiving aggregate did not line up, the command would never arrive.
//!
//! What the saga writes down is checked in full rather than by counting one row
//! type: a saga persists its decisions and the positions of the inputs it read,
//! and nothing else is durable.  Reading the whole stream back is what makes a
//! stray framework row a failure here rather than something a narrower filter
//! would step over.
//!
//! Run with: `cargo test -p nitinol-saga --test e2e_saga`.

#[path = "common/helpers.rs"]
mod common;
use common::{readable_consumed_fact_of, ConsumedFactPayload, JsonCodec, CONSUMED_MARKER};

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Decider, Decision, Event, Query,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    EventStore, InMemoryCheckpointStore, InMemoryEventStore, InMemoryStore,
};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaDelivery, SagaDeliveryProps, SagaEffect, SagaId, SagaProps,
    SagaSubscribe, Subscription,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.saga"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.saga"), TypeName::new("Reserved"));
    type Codec = JsonCodec;
}

/// The saga's own decision: this order is to be reserved, and where.
///
/// The target stream key rides in the decision because the mapping is handed
/// the decision and nothing else — a delivery host keeps no saga resident, so
/// anything it needs to address a command has to be written down here.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
    inventory: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.saga"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
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

#[derive(Default)]
struct Inventory {
    reserved: Vec<String>,
}

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, event: Reserved) {
        self.reserved.push(event.sku);
    }
}

/// The command the saga's mapping produces.
///
/// It carries neither `Clone` nor a serializer: a delivery host writes no copy
/// of a command down, so a command needs nothing beyond what issuing it and
/// awaiting its answer already asks for.
struct Reserve {
    sku: String,
}

/// Idempotent receive: the SKU is the operation id this domain recognises a
/// repeat by.
///
/// Delivery is at-least-once, so a receiver that counted every arrival would
/// make the assertions below hostage to a redelivery the host is entitled to —
/// the contract under test is "the command arrived", not "it arrived once".
impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        if self.reserved.contains(&cmd.sku) {
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![Reserved { sku: cmd.sku }]).output(())
    }
}

/// Every SKU this inventory has reserved, in the order it did.
struct ReservedSkus;

impl Query<ReservedSkus> for Inventory {
    type Response = Vec<String>;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: ReservedSkus) -> Result<Vec<String>, Self::Error> {
        Ok(self.reserved.clone())
    }
}

/// What one `Saga::handle` invocation saw.
///
/// The SKU identifies *which* upstream record reached it, which is the whole
/// question the correlation scenario asks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedContext {
    saga_id: String,
    sequence: u64,
    sku: String,
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
    /// Stream key of the Inventory aggregate every decision is addressed to.
    inventory: String,
    captured: Arc<Mutex<Vec<CapturedContext>>>,
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
                saga_id: ctx.saga_id().as_str().to_owned(),
                sequence: ctx.sequence(),
                sku: event.sku.clone(),
            });

        SagaEffect::persist(ReservationRequested {
            sku: event.sku,
            inventory: self.inventory.clone(),
        })
    }
}

/// What a `ReservationRequested` implies for everybody else, shared by both
/// sagas for the same reason [`ReservationWork`] is: the two differ in their
/// correlation rule and in nothing else, and `Saga::dispatch` is an associated
/// function, so the one mapping is written here and named by both impls.
fn reservation_deliveries(event: &ReservationRequested) -> Vec<SagaDelivery> {
    vec![SagaDelivery::new::<Inventory, Reserve>(
        AggregateId::new(event.inventory.clone()),
        Reserve {
            sku: event.sku.clone(),
        },
    )]
}

struct ReservationSaga(ReservationWork);

#[async_trait]
impl Saga for ReservationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(RESERVATION_SAGA_ID))
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        reservation_deliveries(event)
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
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
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => event
                .sku
                .starts_with("MATCH-")
                .then(|| SagaId::new(MATCH_ONLY_SAGA_ID)),
        }
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        reservation_deliveries(event)
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        Ok(self.0.reserve(event, ctx))
    }
}

/// How long a positive claim is given before the test calls it a failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a probe re-reads what it is waiting for, and how often the
/// delivery host asks its store for what is new.
const PROBE_INTERVAL: Duration = Duration::from_millis(25);

fn captured_snapshot(captured: &Arc<Mutex<Vec<CapturedContext>>>) -> Vec<CapturedContext> {
    captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_until_handled(
    captured: &Arc<Mutex<Vec<CapturedContext>>>,
    want: usize,
) -> Vec<CapturedContext> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let seen = captured_snapshot(captured);
        if seen.len() >= want {
            return seen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {want} Saga::handle invocations; saw {seen:?}"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Every SKU the inventory has reserved, once it holds at least one.
async fn wait_until_reserved(proxy: &AggregateProxy<Inventory>) -> Vec<String> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let reserved = proxy
            .exec(ReservedSkus)
            .await
            .expect("exec(ReservedSkus) must succeed");
        if !reserved.is_empty() {
            return reserved;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the saga's decision must become a Reserve command at Inventory"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// The saga's own stream, once it holds the fact settling an input.
async fn wait_until_settled(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.iter().any(|e| e.event_type == CONSUMED_MARKER) {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a `{CONSUMED_MARKER}` row on the saga's own \
             stream; it held {:?}",
            describe(&events)
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect()
}

#[tokio::test]
async fn aggregate_event_drives_saga_to_command_target_aggregate() {
    let ps = ProcessSystem::new().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(Arc::clone(&journal))
                .build(),
        ))
        .build();

    let order_id = AggregateId::new("saga-e2e-order");
    let order_proxy = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let inventory_id = AggregateId::new("saga-e2e-inventory");
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(inventory_id.clone())
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RESERVATION_SAGA_ID);
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));

    let inventory_key_for_producer = inventory_id.as_str().to_owned();
    let captured_for_producer = Arc::clone(&captured);

    let _saga_proxy = SagaProps::<ReservationSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| {
            ReservationSaga(ReservationWork {
                inventory: inventory_key_for_producer.clone(),
                captured: Arc::clone(&captured_for_producer),
            })
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&journal, &order_id))
    .spawn(system.process_system())
    .await;

    // The host's whole input is the saga's own store, plus the slot it records
    // its progress in.  It delivers through `system`, which is how a receiving
    // aggregate is reached by the same reference any other caller would resolve
    // — `Inventory` implements nothing of the host's.
    let saga_persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&saga_store))
            .delivery_checkpoints(InMemoryCheckpointStore::default())
            .build(),
    );
    let _delivery_host =
        SagaDeliveryProps::<ReservationSaga, InMemoryCheckpointStore>::new(&saga_persistence)
            .with_poll_interval(PROBE_INTERVAL)
            .spawn(&system)
            .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-001".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    assert_eq!(
        wait_until_handled(&captured, 1).await,
        vec![CapturedContext {
            saga_id: RESERVATION_SAGA_ID.to_owned(),
            sequence: 0,
            sku: "SKU-001".to_owned(),
        }],
        "the upstream event must reach Saga::handle exactly once, as the \
         instance Saga::correlate named, and a fresh saga is at sequence 0 \
         before its first SagaEffect::Persist"
    );

    assert_eq!(
        wait_until_reserved(&inventory_proxy).await,
        vec!["SKU-001".to_owned()],
        "the decision the saga persisted must become the command its mapping \
         names, at the aggregate the decision addressed — the saga itself never \
         reaches Inventory, so this is the delivery host carrying out what \
         Saga::dispatch answered"
    );

    let saga_events = wait_until_settled(&saga_store, &saga_id).await;

    assert_eq!(
        describe(&saga_events),
        vec![
            format!("{}@1", ReservationRequested::EVENT_TYPE),
            format!("{CONSUMED_MARKER}@2"),
        ],
        "a saga persists its decisions and the positions of the inputs they \
         were decided from, and nothing else: the decision leads the atomic \
         batch at sequence 1 and the fact settling its input closes it at \
         sequence 2"
    );

    assert_eq!(
        saga_events
            .iter()
            .filter_map(readable_consumed_fact_of)
            .collect::<Vec<_>>(),
        vec![ConsumedFactPayload {
            source: order_id.as_str().to_owned(),
            sequence: 1,
            source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
        }],
        "the fact names the stream the input was read from, the sequence it \
         reached and the declared source it settled — without all three a later \
         incarnation could not tell what it may skip"
    );
}

#[tokio::test]
async fn saga_skips_events_that_correlate_to_no_instance() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let publisher_id = AggregateId::new("publisher-A");
    append_order_placed(&upstream_store, &publisher_id, 1, "SKIP-001").await;
    append_order_placed(&upstream_store, &publisher_id, 2, "MATCH-001").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<CapturedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_producer = Arc::clone(&captured);

    let _saga_proxy = SagaProps::<MatchOnlyReservationSaga>::new(
        SagaId::new(MATCH_ONLY_SAGA_ID),
        saga_store,
        move |_components| {
            MatchOnlyReservationSaga(ReservationWork {
                inventory: "saga-route-inventory".to_owned(),
                captured: Arc::clone(&captured_for_producer),
            })
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &publisher_id))
    .spawn(system.process_system())
    .await;

    // `MATCH-001` is the barrier rather than a sleep: the poller delivers a
    // stream in ascending order and `ask`s one record at a time, so once seq 2
    // has reached `handle` the fate of the declined seq 1 before it is already
    // decided.
    let handled = wait_until_handled(&captured, 1).await;

    assert_eq!(
        handled,
        vec![CapturedContext {
            saga_id: MATCH_ONLY_SAGA_ID.to_owned(),
            sequence: 0,
            sku: "MATCH-001".to_owned(),
        }],
        "an observed event Saga::correlate maps to no instance belongs to \
         somebody else: it must be ignored silently rather than reach handle, \
         so the matching record is the only invocation"
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("saga event store load must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}
