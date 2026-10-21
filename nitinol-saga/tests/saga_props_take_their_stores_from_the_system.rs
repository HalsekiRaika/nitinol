//! A saga's wiring names what to observe, and the system it is spawned on
//! supplies every store that observing, journalling and delivering needs.
//!
//! ```gherkin
//!   Scenario: a subscription names no store, in either entry point
//!     Given a system over a store that supplies saga offsets
//!     When SagaManagerProps::<S>::new(f)
//!            .observing::<OrderPlaced>(Subscription::stream(&order_id))
//!            .spawn(&system)
//!          and system.host::<S>(f)
//!            .observing::<OrderPlaced>(Subscription::stream(&order_id))
//!            .spawn()
//!          are each the whole wiring of their own system
//!     And an order is placed on that system
//!     Then the saga observes the order_id stream of that system's journal
//!     And its decision is on that same journal
//!     And the delivery host carries the command the decision implies to its
//!         target
//!     And SagaComponents::system() answers a &SystemHandle rather than an
//!         Option
//! ```
//!
//! One scenario, two entry points, and each half runs on a system of its own:
//! a saga declares one delivery identifier for its event type, so two hosts of
//! one saga on one system would be two consumers of one progress record rather
//! than two ways to start the same wiring.
//!
//! What the halves read is the store the system distributes — the order stream
//! it observed, the saga stream its decision landed on, and the inventory its
//! command reached. A subscription that kept a store of its own would be
//! observing a stream nothing writes to, and the clauses below would find
//! nothing.
//!
//! What reaches Inventory is read at the receiver, which no amount of
//! manager-side journalling could produce on its own: the command exists only
//! because a delivery host read the decision off the journal the system
//! supplied it. The delivery contract itself — at-least-once, the offset,
//! holding — belongs to the `saga_delivery_*.rs` tests.
//!
//! The components clause is a type annotation inside the producer rather than a
//! runtime check, because what changed is the answer's type; the flag beside it
//! is there so "a `&SystemHandle` was handed over" is distinguishable from "the
//! producer was never called".
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_props_take_their_stores_from_the_system`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::{EventSourceSystem, SystemHandle};
use nitinol_eventsource::{Aggregate, AggregateProxy, Decider, Decision, Event, Never, Query};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaComponents, SagaContext, SagaDelivery, SagaEffect, SagaHostProxy, SagaId,
    SagaManagerProps, SagaSubscribe, SagaSystemExt, Subscription,
};

// Fixtures: codec

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

// Fixtures: the stream the saga observes

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.props.from.system"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
}

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;
    type Recover = Never;

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

// Fixtures: the saga's own decision

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.props.from.system"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

// Fixtures: the target the decision is delivered to

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.props.from.system"),
        TypeName::new("Reserved"),
    );
    type Codec = JsonCodec;
}

#[derive(Default)]
struct Inventory {
    reserved: Vec<String>,
}

impl Aggregate for Inventory {
    type Event = Reserved;
    type Recover = Never;

    fn apply(&mut self, event: Reserved) {
        self.reserved.push(event.sku);
    }
}

struct Reserve {
    sku: String,
}

/// Idempotent receive: delivery is at-least-once, so what is under test is that
/// the command arrived, not how often.
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

struct ReservedSkus;

impl Query<ReservedSkus> for Inventory {
    type Response = Vec<String>;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: ReservedSkus) -> Result<Vec<String>, Self::Error> {
        Ok(self.reserved.clone())
    }
}

// Fixtures: the saga

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

struct ReservationSaga;

#[async_trait]
impl Saga for ReservationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Inventory, Reserve>(
            AggregateId::new(INVENTORY_KEY),
            Reserve {
                sku: event.sku.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
    }
}

/// The one correlation every observed order falls under.
const SAGA_ID: &str = "saga-props-from-system-reservation";

/// Stream key of the Order the saga observes.
const ORDER_KEY: &str = "saga-props-from-system-order";

/// Stream key of the Inventory every decision is addressed to.
const INVENTORY_KEY: &str = "saga-props-from-system-inventory";

/// A producer that records having been handed the hosting system.
///
/// The annotation is the contract: a `system()` answering an `Option` has no
/// coercion to `&SystemHandle` and does not compile here.
fn producer(
    saw_system: &Arc<AtomicBool>,
) -> impl Fn(SagaComponents<ReservationSaga>) -> ReservationSaga + Send + Sync + 'static {
    let saw_system = Arc::clone(saw_system);
    move |components| {
        let _hosting: &SystemHandle = components.system();
        saw_system.store(true, Ordering::SeqCst);
        ReservationSaga
    }
}

// Helpers

/// How long a positive claim is given before the test calls it a failure.
///
/// Generous because neither half names a poll interval: the manager's source
/// and the delivery host both run on the cadences the framework defaults to.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a probe re-reads what it is waiting for.
const PROBE_INTERVAL: Duration = Duration::from_millis(20);

/// A system over a store that supplies every slot, including the saga-offset
/// slot a delivery host records its progress in.
async fn in_memory_system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// The SKUs the saga's own stream carries, decoded with the codec its event
/// type names.
async fn requested_skus(journal: &Journal, saga_id: &SagaId) -> Vec<String> {
    let loaded: Vec<_> = journal
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("loading the saga's own stream succeeds")
        .try_collect()
        .await
        .expect("collecting the saga's own stream succeeds");
    loaded
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<ReservationRequested>(&event.payload)
                .expect("the saga's own event is encoded with the codec its type names")
                .sku
        })
        .collect()
}

/// Wait until the saga's own stream carries at least one decision, then answer
/// the SKUs on it.
async fn wait_for_requested(journal: &Journal, saga_id: &SagaId, label: &str) -> Vec<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let skus = requested_skus(journal, saga_id).await;
        if !skus.is_empty() {
            return skus;
        }
        if Instant::now() >= deadline {
            panic!("{label}: the saga's own stream carries no decision");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Wait until `inventory` has reserved at least one SKU, then answer what it
/// holds.
async fn wait_until_reserved(inventory: &AggregateProxy<Inventory>, label: &str) -> Vec<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let reserved = inventory
            .exec(ReservedSkus)
            .await
            .expect("querying the inventory succeeds");
        if !reserved.is_empty() {
            return reserved;
        }
        if Instant::now() >= deadline {
            panic!("{label}: the inventory reserved nothing");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Place `sku` on the system's own Order stream, then read what the saga wiring
/// on that system did with it.
///
/// `host` is whatever the entry point under test handed back, so the clauses
/// below are the same for both of them.
async fn observe(
    system: &EventSourceSystem<InMemoryStore>,
    host: &SagaHostProxy<ReservationSaga>,
    saw_system: &Arc<AtomicBool>,
    sku: &str,
    entry_point: &str,
) {
    let order_id = AggregateId::new(ORDER_KEY);
    let order = system.spawn_aggregate::<Order>(order_id).await;
    let inventory = system.aggregate_proxy::<Inventory>(AggregateId::new(INVENTORY_KEY));

    assert!(
        system
            .process_system()
            .lookup(host.manager().pid())
            .await
            .is_some(),
        "{entry_point}: the manager runs on the process system of the system it \
         was spawned on, so that system's own registry is where it is found"
    );

    order
        .ask(PlaceOrder {
            sku: sku.to_owned(),
        })
        .await
        .expect("asking the order to be placed succeeds");

    assert_eq!(
        wait_for_requested(
            &system.persistence().event_store(),
            &SagaId::new(SAGA_ID),
            &format!(
                "{entry_point}: the saga must observe the order stream of the system's journal"
            )
        )
        .await,
        vec![sku.to_owned()],
        "{entry_point}: the subscription named a stream key and no store, so the \
         order it observed and the decision it journalled are both on the journal \
         the system distributes"
    );

    assert_eq!(
        wait_until_reserved(
            &inventory,
            &format!("{entry_point}: the delivery host must carry the decision to its target")
        )
        .await,
        vec![sku.to_owned()],
        "{entry_point}: the command exists only because the host read the \
         decision off the journal the system supplied it"
    );

    assert!(
        saw_system.load(Ordering::SeqCst),
        "{entry_point}: the instance that decided was built by the producer, so \
         the hosting system it was handed is not an absent one"
    );
}

// Scenario: a subscription names no store, in either entry point

/// Both saga entry points take the journal, the saga offsets and the observed
/// source's store from the system they are spawned on.
#[tokio::test]
async fn both_saga_entry_points_take_their_stores_from_the_system() {
    // Given: a system per entry point — one saga declares one delivery
    // identifier, so two hosts of it on one system would share one progress
    // record
    let from_props_system = in_memory_system().await;
    let from_system_system = in_memory_system().await;
    let props_saw_system = Arc::new(AtomicBool::new(false));
    let system_saw_system = Arc::new(AtomicBool::new(false));

    // When: props built with nothing but a producer are spawned on a system
    let from_props = SagaManagerProps::<ReservationSaga>::new(producer(&props_saw_system))
        .observing::<OrderPlaced>(Subscription::stream(&AggregateId::new(ORDER_KEY)))
        .spawn(&from_props_system)
        .await;

    // And: the system's own entry point is the whole wiring of the other
    let from_system = from_system_system
        .host::<ReservationSaga>(producer(&system_saw_system))
        .observing::<OrderPlaced>(Subscription::stream(&AggregateId::new(ORDER_KEY)))
        .spawn()
        .await;

    // Then
    observe(
        &from_props_system,
        &from_props,
        &props_saw_system,
        "SKU-FROM-PROPS",
        "props spawned on a system",
    )
    .await;

    observe(
        &from_system_system,
        &from_system,
        &system_saw_system,
        "SKU-FROM-SYSTEM",
        "props taken from the system",
    )
    .await;
}
