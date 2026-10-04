//! Runnable demonstration of the pattern this crate exists to show:
//! `PlaceOrder` -> one fact event -> a reservation dispatched through the saga's
//! outbox -> the same operation delivered a second time -> one reservation on
//! the stock item's stream.
//!
//! The wiring mirrors what `tests/common/helpers.rs` builds for the integration
//! tests; this binary demonstrates it end to end with self-verifying
//! assertions, in the same shape as
//! `examples/saga/2.fanout-pattern/src/main.rs`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaDefaultStoreExt;

use saga_idempotent_receive::order::{Order, OrderPlaced, PlaceOrder};
use saga_idempotent_receive::saga::ReservationSaga;
use saga_idempotent_receive::stock_item::{Reserve, StockItem, StockReserved};

/// The stock item the order reserves against.
const SKU: &str = "sku-cobalt-ingot";

/// How much the order reserves — the amount a second application would double.
const ORDER_QUANTITY: u64 = 7;

/// Budget for the order to reach the stock item through the outbox.  The manager
/// polls its upstream every 250ms, so this is a generous deadline rather than a
/// synchronisation device: the wait below polls an observable fact.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() {
    let ps = ProcessSystem::new().await;

    // One store for the whole example.  `EventStore` is stream-keyed, so the
    // order's stream, the stock item's stream and the saga's journal are tenants
    // of this one instance; the manager's subscription below is what narrows it
    // back down to the order's stream.  The handle is kept here as well, because
    // the streams are what this demonstration verifies itself against.
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_event_store(Arc::clone(&store))
        .build();

    let order_id = AggregateId::new("order-cobalt-42");
    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;

    // The system is itself the shared handle, so the producer and the
    // crash-restart factory each take a clone of it rather than a shared wrapper
    // around it.
    let system_for_producer = system.clone();
    let system_for_factory = system.clone();

    // One `subscribed_to` call per source the saga's alphabet declares — here
    // the order's stream, read as `OrderPlaced` — and `spawn` exists only once
    // every declared source has one.
    let _manager = system
        .saga_manager_props(move |_components| ReservationSaga::new(system_for_producer.clone()))
        .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
        .with_crash_restart_factory(move |payload: &[u8]| {
            ReservationSaga::crash_restart_intent(&system_for_factory, payload)
        })
        .spawn(system.process_system())
        .await;

    order
        .ask(PlaceOrder {
            order_id: order_id.as_str().to_owned(),
            sku: SKU.to_owned(),
            quantity: ORDER_QUANTITY,
        })
        .await
        .expect("ask(PlaceOrder) must succeed");
    println!("placed {} for {ORDER_QUANTITY} of {SKU}", order_id.as_str());

    let stock_key = StockItem::stream_key(SKU);
    let first = wait_for_first_reservation(&store, &stock_key).await;
    println!(
        "the saga dispatched the reservation; the stock item applied {} for {} unit(s)",
        first.reservation, first.quantity
    );

    // The second delivery of the same operation.  The id is re-derived from the
    // order's own fact event, which is what makes this the same operation arriving
    // again rather than a new one that happens to look alike: a re-dispatch of the
    // saga's pending marker would carry exactly this id, because it is the one the
    // first dispatch carried.
    let reservation = ReservationSaga::reservation_id(&order_fact(&store, order_id.as_str()).await);
    system
        .aggregate_proxy::<StockItem>(AggregateId::new(&stock_key))
        .ask(Reserve {
            reservation: reservation.clone(),
            stock: stock_key.clone(),
            quantity: ORDER_QUANTITY,
        })
        .await
        .expect("a repeat of an applied reservation must be accepted, not refused");
    println!("delivered the operation {reservation} a second time; it was accepted");

    let recorded = reservations(&store, &stock_key).await;
    assert_eq!(
        recorded.len(),
        1,
        "two deliveries of one operation must leave one reservation (recorded {recorded:?})"
    );
    assert_eq!(
        recorded[0].reservation, reservation,
        "the recorded reservation must be the operation both deliveries named"
    );
    assert_eq!(
        recorded[0].quantity, ORDER_QUANTITY,
        "the reserved quantity must be one order's worth"
    );

    println!(
        "idempotent-receive example finished: two deliveries, {} unit(s) reserved once",
        recorded[0].quantity
    );
}

/// Every record on the stream `key`, in stream order.
async fn load_stream(store: &Arc<dyn EventStore>, key: &str) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("load stream must succeed")
        .try_collect()
        .await
        .expect("collect stream must succeed")
}

/// Read a stored record back through the codec its event type names.
///
/// The format belongs to the event, so this reads it from there rather than
/// naming a serializer of its own.
fn decode<E: Event>(event: &LoadedEvent) -> E {
    <E::Codec as Codec<E>>::decode(&event.payload)
        .expect("a record this example wrote must decode with the codec its event names")
}

/// The reservations the stock item's own stream records, in stream order.
async fn reservations(store: &Arc<dyn EventStore>, stock_key: &str) -> Vec<StockReserved> {
    load_stream(store, stock_key)
        .await
        .iter()
        .filter(|event| event.event_type == StockReserved::EVENT_TYPE)
        .map(decode::<StockReserved>)
        .collect()
}

/// The fact event the order recorded — the upstream event the operation id is
/// derived from.
async fn order_fact(store: &Arc<dyn EventStore>, order_key: &str) -> OrderPlaced {
    let events = load_stream(store, order_key).await;
    let placed = events
        .iter()
        .find(|event| event.event_type == OrderPlaced::EVENT_TYPE)
        .expect("the placed order must be recorded on its own stream");
    decode(placed)
}

/// Poll the stock item's stream until the dispatched reservation lands on it.
///
/// Dispatch through the outbox is asynchronous and the stream is where it
/// becomes observable, so this waits on that fact rather than sleeping for a
/// guessed duration.
async fn wait_for_first_reservation(store: &Arc<dyn EventStore>, stock_key: &str) -> StockReserved {
    let deadline = std::time::Instant::now() + DISPATCH_TIMEOUT;
    loop {
        if let Some(first) = reservations(store, stock_key).await.into_iter().next() {
            return first;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "the saga must drive a reservation into the stock item within {DISPATCH_TIMEOUT:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
