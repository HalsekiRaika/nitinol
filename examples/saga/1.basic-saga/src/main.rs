use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::info;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::EventEnvelope;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaId, SagaProps};

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
    let order_proxy = system
        .spawn_aggregate::<Order>(AggregateId::new("example-order"), order_store)
        .await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory_proxy = system
        .spawn_aggregate::<Inventory>(
            AggregateId::new("example-inventory"),
            inventory_store,
        )
        .await;

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<OrderPlaced>>(ProcessName::new("example-saga-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("example-reservation-saga");
    let done = Arc::new(Notify::new());

    let inventory_for_producer = inventory_proxy.clone();
    let done_for_producer = Arc::clone(&done);
    let route_target = saga_id.clone();

    let _saga_proxy = SagaProps::<ReservationSaga>::new(
        saga_id.clone(),
        saga_store,
        move || ReservationSaga {
            inventory: inventory_for_producer.clone(),
            done_notify: Arc::clone(&done_for_producer),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), move |_event: &OrderPlaced| {
        Some(route_target.clone())
    })
    .spawn(system.process_system())
    .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-EXAMPLE".into(),
            stream: stream.clone(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    tokio::time::timeout(Duration::from_millis(500), done.notified())
        .await
        .expect("saga must drive a Reserve into Inventory within 500 ms");

    let count = inventory_proxy
        .exec(GetReservedCount)
        .await
        .expect("exec(GetReservedCount) must succeed");
    assert_eq!(count, 1, "Inventory must have received exactly one Reserve");

    info!(reserved_count = count, "saga example finished successfully");
}
