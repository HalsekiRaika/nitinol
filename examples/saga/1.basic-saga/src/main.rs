use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaSystemExt;

use saga_basic_saga::inventory::{GetReservedCount, Inventory};
use saga_basic_saga::order::{Order, OrderPlaced, PlaceOrder};
use saga_basic_saga::saga::ReservationSaga;

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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::in_memory())
        .build();

    let order_id = AggregateId::new("example-order");
    let order_proxy = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(AggregateId::new("example-inventory"))
        .await;

    let inventory_for_producer = inventory_proxy.clone();

    // The producer is handed the capabilities the framework resolved for this
    // saga; this one needs none of them, so it ignores them.  It is called again
    // if the runtime restarts the process, which is why the `AggregateProxy` it
    // captures is a reference that can be re-acquired rather than a live
    // activation.
    //
    // One `subscribed_to` call per source the alphabet declares — here the
    // order's stream, read as `OrderPlaced` — and `spawn` exists only once every
    // declared source has one.
    let _saga_proxy = system
        .spawn_saga(ReservationSaga::instance_id(), move |_components| {
            ReservationSaga {
                inventory: inventory_for_producer.clone(),
            }
        })
        .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
        .spawn()
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-EXAMPLE".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    // Poll until Inventory has processed the Reserve command dispatched by the saga.
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
    assert_eq!(count, 1, "Inventory must have received exactly one Reserve");

    info!(reserved_count = count, "saga example finished successfully");
}
