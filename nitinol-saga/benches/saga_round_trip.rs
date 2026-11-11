//! What one trip through a saga costs: an order is placed, the saga decides a
//! reservation, and a delivery host carries it to the inventory.
//!
//! The number here is a baseline rather than a target. Nothing in this file is
//! a claim about what a round trip ought to cost — it is what it costs today,
//! so that a later change can be told apart from noise.
//!
//! # Both cadences are named, and both are in the name
//!
//! Everything between the order's own fact and the command landing on the
//! inventory is carried by two pull consumers: the manager's subscription to
//! the order's stream, and the delivery host's subscription to the saga's own
//! decisions. A round trip therefore waits out both of their intervals, and
//! with the framework's defaults it would be those intervals and nothing else.
//!
//! So this benchmark names both — one millisecond each — and the name
//! `saga_round_trip/poll_1ms` says which cadence the number belongs to. A
//! number taken at another cadence is a different measurement and not a
//! regression.
//!
//! # What the iterations do to each other
//!
//! One system, one order, one inventory and one saga, for every iteration: a
//! manager started per iteration would leave its one-millisecond pollers
//! running behind it and the iterations after it would be measured against that
//! load.
//!
//! The cost of that is drift. Every iteration writes to the one in-memory
//! journal — the order's fact, the saga's decision, the reservation, and the
//! positions the framework records for itself — and this store walks everything
//! it holds on every load. So a later iteration is read out of a fuller store
//! than an earlier one, and the median is over a run that got steadily slower.
//! How far it drifts is bounded by the group's `sample_size` and
//! `measurement_time` and by nothing else, which is why both are set low here.
//!
//! Run with `cargo bench -p nitinol-saga`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Aggregate, AggregateProxy, Decider, Decision, Event, Never, Query};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaDelivery, SagaEffect, SagaHostProxy, SagaId, SagaSubscribe,
    SagaSystemExt, Subscription,
};
use tokio::runtime::Runtime;

/// Named rather than defaulted, and named the same on both sides, so the
/// number this benchmark reports is one cadence and not a mixture.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Low, with the measurement below, for the reason the crate-level note gives:
/// every iteration leaves records in the one store the next iteration reads.
const WARM_UP: Duration = Duration::from_millis(500);

const MEASUREMENT: Duration = Duration::from_secs(1);

const ROUND_TRIP_DEADLINE: Duration = Duration::from_secs(30);

const ORDER_KEY: &str = "bench-saga-order";

const INVENTORY_KEY: &str = "bench-saga-inventory";

const SAGA_KEY: &str = "bench-saga-reservation";

const SKU: &str = "BENCH-SKU";

/// The round trip of the k-th order is finished when the inventory has made k
/// reservations, so this is also how the completion of one iteration is told
/// from the completion of the one before it.
static PLACED: AtomicU64 = AtomicU64::new(0);

struct JsonCodec;

impl<E> Codec<E> for JsonCodec
where
    E: Serialize + for<'de> Deserialize<'de>,
{
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
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("bench.saga"), TypeName::new("OrderPlaced"));

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

/// The saga's own decision, and the whole of what a delivery host reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    inventory: String,
    sku: String,
    reservation: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("bench.saga"),
        TypeName::new("ReservationRequested"),
    );

    type Codec = JsonCodec;
}

/// The fact a carried-out reservation leaves behind.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved {
    sku: String,
    reservation: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("bench.saga"), TypeName::new("Reserved"));

    type Codec = JsonCodec;
}

#[derive(Default)]
struct Inventory {
    reserved_count: u64,
    reservations: Vec<String>,
}

impl Aggregate for Inventory {
    type Event = Reserved;
    type Recover = Never;

    fn apply(&mut self, event: Reserved) {
        self.reserved_count += 1;
        self.reservations.push(event.reservation);
    }
}

struct Reserve {
    sku: String,
    reservation: String,
}

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        // Idempotent receive: delivery is at-least-once, so the same
        // reservation arriving twice is accepted with nothing written.
        if self
            .reservations
            .iter()
            .any(|made| made == &cmd.reservation)
        {
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![Reserved {
            sku: cmd.sku,
            reservation: cmd.reservation,
        }])
        .output(())
    }
}

struct GetReservedCount;

impl Query<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetReservedCount) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

struct ReservationSaga {
    inventory: AggregateId,
}

#[async_trait]
impl Saga for ReservationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_KEY))
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Inventory, Reserve>(
            AggregateId::new(event.inventory.clone()),
            Reserve {
                sku: event.sku.clone(),
                reservation: event.reservation.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;

        // The operation id is the upstream record's own position, so a
        // redelivered order decides the same reservation and the inventory
        // recognises it as a repeat.
        let reservation = format!(
            "{}#{}",
            ctx.upstream_aggregate_id().as_str(),
            ctx.upstream_sequence()
        );

        Ok(SagaEffect::persist(ReservationRequested {
            inventory: self.inventory.as_str().to_owned(),
            sku: event.sku,
            reservation,
        }))
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a benchmark run needs a tokio runtime to drive the system on")
}

/// Everything one run of this benchmark is driven through.
///
/// The system and the saga handle are in here because the run holds them, not
/// because it reads them: the two aggregate references are what the round trip
/// is placed on and observed through.
type Wiring = (
    EventSourceSystem<InMemoryStore>,
    AggregateProxy<Order>,
    AggregateProxy<Inventory>,
    SagaHostProxy<ReservationSaga>,
);

/// The whole process, started once: one store, the two aggregates, and the
/// saga that joins them.
async fn wiring() -> Wiring {
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let order_id = AggregateId::new(ORDER_KEY);
    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let inventory_id = AggregateId::new(INVENTORY_KEY);
    let inventory = system
        .spawn_aggregate::<Inventory>(inventory_id.clone())
        .await;

    // One declaration starts the manager and the delivery host that carries out
    // what it decided, and names the cadence of each.
    let addressed = inventory_id.clone();
    let host = system
        .host::<ReservationSaga>(move |_components| ReservationSaga {
            inventory: addressed.clone(),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_id).with_poll_interval(POLL_INTERVAL))
        .with_delivery_poll_interval(POLL_INTERVAL)
        .spawn()
        .await;

    (system, order, inventory, host)
}

async fn reserved_to(inventory: &AggregateProxy<Inventory>, expected: u64) {
    let watch = async {
        loop {
            let made = inventory
                .exec(GetReservedCount)
                .await
                .expect("querying the inventory must be answered");
            if made >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(ROUND_TRIP_DEADLINE, watch)
        .await
        .unwrap_or_else(|_| {
            panic!("the delivery host must land reservation {expected} on the inventory")
        });
}

/// What one trip from a placed order to a reservation on the inventory costs.
///
/// Timed: the `ask` that places the order, the manager reading it off its
/// subscription, the decision it journals, the delivery host reading that
/// decision, the `ask(Reserve)` it carries to the inventory, and the query that
/// observes the result. Both poll intervals are inside that window, and with a
/// cadence of one millisecond each they are most of what a fast round trip is.
///
/// The query that observes the result is in the window too, and it is asked in
/// a loop: the inventory is polled until it answers with the k-th reservation,
/// so what is measured includes however many answers it gave before that one.
///
/// Not timed: the wiring, which is started once for the whole run.
fn saga_round_trip(c: &mut Criterion) {
    let rt = runtime();
    let (_system, order, inventory, _host) = rt.block_on(wiring());
    let order = &order;
    let inventory = &inventory;

    let mut group = c.benchmark_group("saga_round_trip");
    group.sample_size(10);
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    group.bench_function("poll_1ms", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let mut measured = Duration::ZERO;
            for _ in 0..iters {
                let placed = PLACED.fetch_add(1, Ordering::Relaxed) + 1;

                let started = Instant::now();
                order
                    .ask(PlaceOrder {
                        sku: SKU.to_owned(),
                    })
                    .await
                    .expect("asking the order to be placed must be answered");
                reserved_to(inventory, placed).await;
                measured += started.elapsed();
            }
            measured
        });
    });

    group.finish();
}

criterion_group!(saga, saga_round_trip);
criterion_main!(saga);
