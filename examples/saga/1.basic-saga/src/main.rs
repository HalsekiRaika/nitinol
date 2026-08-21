use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaDefaultStoreExt;

use saga_basic_saga::codec::JsonCodec;
use saga_basic_saga::inventory::{GetReservedCount, Inventory};
use saga_basic_saga::order::{Order, PlaceOrder};
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

    // One store for the whole example.  `EventStore` is stream-keyed, so the
    // order's stream, the inventory's stream and the saga's own journal are
    // tenants of this one instance, each under its own key — which is what lets
    // the system hand it to every spawn below.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(store)
        .build();

    let order_id = AggregateId::new("example-order");
    let order_proxy = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(AggregateId::new("example-inventory"))
        .await;

    let inventory_for_producer = inventory_proxy.clone();

    let _saga_proxy = system
        .spawn_saga(ReservationSaga::instance_id(), move || ReservationSaga {
            inventory: inventory_for_producer.clone(),
        })
        .subscribed_to(system.subscription(&order_id))
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
