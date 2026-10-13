//! The wiring of the basic saga example, end to end.
//!
//! Three spawns make the process work, and each is one of the three parts the
//! crate docs describe:
//!
//! * the two aggregates — the order whose fact starts the process, and the
//!   inventory the reservation is addressed to;
//! * the saga, with one `observing` call per source its alphabet declares;
//! * the **delivery host**, which is what turns the saga's persisted decisions
//!   into commands at the inventory.  Without it the saga still decides and
//!   still journals, and nothing is ever delivered.
//!
//! Run with:
//!   cargo run -p saga-basic-saga

use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryExt, SagaSystemExt};

use saga_basic_saga::inventory::{GetReservedCount, Inventory};
use saga_basic_saga::order::{Order, OrderPlaced, PlaceOrder};
use saga_basic_saga::saga::ReservationSaga;

/// How often the delivery host asks the store what is new.
///
/// A pull consumer trades latency for load on the store; an example wants the
/// latency, so it polls far more eagerly than a deployment would.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let ps = ProcessSystem::new().await;

    // One supplier for the whole example.  `EventStore` is stream-keyed, so the
    // order's stream, the inventory's stream and the saga's own journal are
    // tenants of the one journal it supplies, each under its own key — which is
    // what lets every spawn below resolve the same store.
    //
    // `InMemoryStore` also supplies a delivery-checkpoint slot, which is the
    // second thing a delivery host needs: a store with no such slot makes
    // `deliver` below a compile error rather than a host that cannot record
    // where it got to.
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let order_id = AggregateId::new("example-order");
    let order_proxy = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let inventory_id = AggregateId::new("example-inventory");
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(inventory_id.clone())
        .await;

    // `host` is the one verb a saga is started with, and it always starts a
    // manager.  `ReservationSaga::correlate` answers the same id for every
    // order, so what runs is that manager with a single instance — the
    // degenerate case rather than a different kind of spawn, which is why no
    // `SagaId` is named here.
    //
    // The factory is handed the capabilities the framework resolved for this
    // saga; this one needs none of them, so it ignores them.  It is called
    // again for every instance and whenever the runtime rebuilds one, so what
    // it captures must be re-acquirable — an `AggregateId` trivially is, being
    // the address of the inventory rather than a live reference to it.
    //
    // One `observing` call per source the alphabet declares — here the order's
    // stream, read as `OrderPlaced` — and `spawn` exists only once every
    // declared source has one.
    let saga_inventory = inventory_id.clone();
    let _saga_proxy = system
        .host::<ReservationSaga>(move |_components| ReservationSaga {
            inventory: saga_inventory.clone(),
        })
        .observing::<OrderPlaced>(system.subscription(&order_id))
        .spawn()
        .await;

    // The delivery host, started by `deliver` — symmetric with `host`.  It
    // subscribes to the decisions every instance of `ReservationSaga` has
    // persisted — its input is the store, so it needs nothing of the saga
    // process and keeps no saga resident — asks `ReservationSaga::dispatch`
    // what each one implies, and `ask`s the addressed aggregate.  The handle is
    // only how an operator reaches the running host; dropping it would not
    // stop it.
    let _delivery = system
        .deliver::<ReservationSaga>()
        .with_poll_interval(POLL_INTERVAL)
        .spawn()
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-EXAMPLE".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    // Everything after the order's own fact is asynchronous: the saga has to see
    // it on its subscription, decide, and journal, and only then does the host
    // read that decision and deliver it.  So the inventory is polled rather than
    // awaited.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let count = loop {
        let c = inventory_proxy
            .exec(GetReservedCount)
            .await
            .expect("exec(GetReservedCount) must succeed");
        if c >= 1 {
            break c;
        }
        if std::time::Instant::now() >= deadline {
            panic!("the delivery host must land a Reserve on Inventory within 5 seconds");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    assert_eq!(
        count, 1,
        "one order means one reservation: the inventory recognises a redelivered \
         reservation by its operation id and writes nothing for it"
    );

    info!(reserved_count = count, "saga example finished successfully");
}
