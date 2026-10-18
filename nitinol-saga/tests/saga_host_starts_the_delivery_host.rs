//! One `host` declaration starts the manager and the delivery host that
//! carries out what it decided.
//!
//! ```gherkin
//!   Scenario: one declaration, and the decision reaches its target
//!     Given a system over a store that supplies saga offsets, an Order
//!           aggregate and an Inventory aggregate
//!     When `host` is the only wiring, with no delivery setting named
//!     And an order is placed
//!     Then the command the saga's mapping names reaches Inventory
//!     And no delivery host is wired anywhere in this file
//!
//!   Scenario: the handle reaches both halves
//!     Given the handle that one `host` declaration handed back
//!     Then the manager it names is a process of that system
//!     And the delivery host it names carries the identifier derived from the
//!         saga's declared event type, over the single partition a wiring that
//!         names no count gets
//! ```
//!
//! A manager journals its decisions and nothing else: the commands they imply
//! reach their targets only because a delivery host reads that journal. Wiring
//! the two separately made "the manager is running" and "its decisions are
//! being carried out" two different facts, with nothing to say the second was
//! missing. So the clause this file fixes is that one declaration settles both —
//! what reaches Inventory below is read at the receiver, which no amount of
//! manager-side journalling could produce on its own.
//!
//! Nothing about delivery is configured here. That is deliberate: the scenario
//! is the shortest wiring a saga has, so the host runs on its own defaults, and
//! the second clause reads the two of them a handle can be asked for.
//!
//! The delivery contract itself — at-least-once, the offset, holding, the
//! operator's way past a hold — belongs to `SagaDeliveryProps` and is fixed by
//! the `saga_delivery_*.rs` tests. What this file adds is that `host` starts a
//! host that keeps it.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_host_starts_the_delivery_host`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Aggregate, AggregateProxy, Decider, Decision, Event, Never, Query};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaDelivery, SagaDeliveryError, SagaEffect, SagaId, SagaSubscribe,
    SagaSystemExt, Subscription,
};

// Fixtures: an upstream aggregate, a saga that reacts to it, and the receiver
// its decision is addressed to.

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.host.delivers"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
}

/// The saga's own decision — the stream the delivery host consumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.host.delivers"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.host.delivers"), TypeName::new("Reserved"));
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

/// The command the saga's mapping produces.
struct Reserve {
    sku: String,
}

/// Idempotent receive: the SKU is the operation id this domain recognises a
/// repeat by.
///
/// Delivery is at-least-once, so a receiver that recorded every arrival would
/// make the clause below hostage to a redelivery the host is entitled to — what
/// is under test is that the command arrived, not that it arrived once.
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

/// Stream key of the Inventory every decision is addressed to.
///
/// A constant rather than something the saga captures: the mapping is an
/// associated function handed the decision and nothing else, and what this file
/// is about is the wiring rather than how a target is chosen.
const INVENTORY_KEY: &str = "saga-host-delivers-inventory";

/// Stream key of the Order the saga observes.
const ORDER_KEY: &str = "saga-host-delivers-order";

/// The one correlation every observed order falls under.
const SAGA_ID: &str = "saga-host-delivers-reservation";

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

// Helpers

/// How long a positive claim is given before the test calls it a failure.
///
/// Generous because the wiring under test names no poll interval: both the
/// manager's source and the delivery host run on the cadences the framework
/// defaults to.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a probe re-reads what it is waiting for.
const PROBE_INTERVAL: Duration = Duration::from_millis(20);

/// A system over a store that supplies every slot, including the saga-offset
/// slot a delivery host records its progress in.
async fn system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

/// Wait until `proxy` has reserved at least one SKU, then answer what it holds.
async fn wait_until_reserved(proxy: &AggregateProxy<Inventory>, label: &str) -> Vec<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let reserved = proxy
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

#[tokio::test]
async fn one_host_declaration_carries_a_decision_to_its_target() {
    // Given
    let system = system().await;
    let order_id = AggregateId::new(ORDER_KEY);
    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;
    let inventory = system.aggregate_proxy::<Inventory>(AggregateId::new(INVENTORY_KEY));

    // When: one declaration, and nothing about delivery said at all
    let _host = system
        .host::<ReservationSaga>(|_components| ReservationSaga)
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn()
        .await;

    order
        .ask(PlaceOrder {
            sku: "SKU-HOST-001".into(),
        })
        .await
        .expect("asking the order to be placed succeeds");

    // Then
    assert_eq!(
        wait_until_reserved(
            &inventory,
            "one `host` declaration must leave the decision delivered"
        )
        .await,
        vec!["SKU-HOST-001".to_owned()],
        "the saga reaches Inventory through nothing of its own: the decision it \
         journalled became the command its mapping names only because `host` \
         started the delivery host beside the manager"
    );
}

#[tokio::test]
async fn the_handle_reaches_the_manager_and_the_delivery_host() {
    // Given
    let system = system().await;
    let order_id = AggregateId::new(ORDER_KEY);

    let host = system
        .host::<ReservationSaga>(|_components| ReservationSaga)
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn()
        .await;

    // Then: the manager half is a process of the system the props came from
    assert!(
        system
            .process_system()
            .lookup(host.manager().pid())
            .await
            .is_some(),
        "the manager the handle names runs on the system `host` was called on, \
         so that system's own registry is where its process is found"
    );

    // And: the delivery half is reachable, and is the one this wiring started
    assert_eq!(
        host.delivery().delivery_id().as_str(),
        <ReservationRequested as Event>::EVENT_TYPE
            .to_path()
            .to_string(),
        "a wiring that names no identifier leaves the host filing its progress \
         under the one derived from the event type the saga declares"
    );

    let refusal = host
        .delivery()
        .advance_offset(1, 1)
        .await
        .expect_err("a host that names no partition count has partition 0 and no other");
    assert!(
        matches!(
            refusal,
            SagaDeliveryError::NoSuchPartition {
                partition: 1,
                partitions: 1,
            }
        ),
        "the operator entry reaches the partitions this handle's host actually \
         started — one of them, because the wiring named no count; got {refusal:?}"
    );
}
