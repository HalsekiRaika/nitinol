use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaProps;

use saga_basic_saga::codec::JsonCodec;
use saga_basic_saga::inventory::{GetReservedCount, Inventory};
use saga_basic_saga::order::{Order, OrderPlaced, PlaceOrder};
use saga_basic_saga::saga::{ReservationRequested, ReservationSaga};

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
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("example-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&order_store))
        .await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(AggregateId::new("example-inventory"), inventory_store)
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let inventory_for_producer = inventory_proxy.clone();

    let _saga_proxy =
        SagaProps::<ReservationSaga>::new(ReservationSaga::instance_id(), saga_store, move || {
            ReservationSaga {
                inventory: inventory_for_producer.clone(),
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
