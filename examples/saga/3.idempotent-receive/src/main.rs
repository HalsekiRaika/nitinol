//! Runnable demonstration of the pattern this crate exists to show:
//! `PlaceOrder` -> one fact event -> one saga decision -> a delivery host turns
//! that decision into a `Reserve` at the stock item -> the same operation
//! delivered a second time -> one reservation on the stock item's stream.
//!
//! The wiring mirrors what `tests/common/helpers.rs` builds for the integration
//! tests; this binary demonstrates it end to end with self-verifying
//! assertions.

use std::time::Duration;

use futures_util::TryStreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaSystemExt, Subscription};

use saga_idempotent_receive::order::{Order, OrderPlaced, PlaceOrder};
use saga_idempotent_receive::saga::ReservationSaga;
use saga_idempotent_receive::stock_item::{Reserve, StockItem, StockReserved};

/// The stock item the order reserves against.
const SKU: &str = "sku-cobalt-ingot";

/// How much the order reserves — the amount a second application would double.
const ORDER_QUANTITY: u64 = 7;

/// How often the delivery host asks the store what the saga has decided.
///
/// A pull consumer trades latency for load on the store; short here because the
/// demonstration waits on the host's work.
const DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Budget for the order to reach the stock item through the delivery host.  This
/// is a generous deadline rather than a synchronisation device: the wait below
/// polls an observable fact.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() {
    let ps = ProcessSystem::new().await;

    // One supplier for the whole example, naming two slots.
    //
    // `EventStore` is stream-keyed, so the order's stream, the stock item's
    // stream and the saga's journal are tenants of this one instance; the
    // manager's subscription narrows it back down to the order's stream, and the
    // delivery host's narrows it down to the saga's own decision event type.
    // How far the host has got is not a stream at all, so it is a slot of its
    // own: without it there is no `deliver` on this system to call, which is
    // the wiring being told so while it is compiled rather than when the host
    // first tries to record something. `InMemoryStore` supplies every slot,
    // that one included.
    //
    // The journal is taken back off the supplier below, because the streams are
    // what this demonstration verifies itself against — and what a supplier
    // answers with is the store it was built on rather than a region of its
    // own.
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let store = system.persistence().event_store();

    let order_id = AggregateId::new("order-cobalt-42");
    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;

    // One `observing` call per source the saga's alphabet declares — here the
    // order's stream, read as `OrderPlaced` — and `spawn` exists only once
    // every declared source has one.
    //
    // The saga holds nothing, because it sends nothing: its only effect is the
    // decision it persists, so the factory needs none of the capabilities it is
    // handed.
    //
    // The same declaration starts the delivery host, which is what carries a
    // decision anywhere.  Its whole input is the store: it subscribes to the
    // decision event type `ReservationSaga` declares, wherever an instance of it
    // wrote one, so nothing about which instances exist is named for it — only
    // how often it asks the store.  Without it the saga would still decide and
    // still journal, and no stock item would ever hear about it, which is why
    // hosting a saga is not something a wiring can do half of.
    let _host = system
        .host::<ReservationSaga>(|_| ReservationSaga)
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .with_delivery_poll_interval(DELIVERY_POLL_INTERVAL)
        .spawn()
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
        "the host delivered the reservation; the stock item applied {} for {} unit(s)",
        first.reservation, first.quantity
    );

    // The second delivery of the same operation.  The id is re-derived from the
    // order's own fact event, which is what makes this the same operation
    // arriving again rather than a new one that happens to look alike: a host
    // resuming from an offset that never moved re-reads the saga's decision
    // and re-derives exactly this id, because the id is on that record.
    let reservation = ReservationSaga::reservation_id(&order_fact(&store, order_id.as_str()).await);
    system
        .aggregate_proxy::<StockItem>(AggregateId::new(&stock_key))
        .ask(Reserve {
            reservation: reservation.clone(),
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
async fn load_stream(store: &Journal, key: &str) -> Vec<LoadedEvent> {
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
async fn reservations(store: &Journal, stock_key: &str) -> Vec<StockReserved> {
    load_stream(store, stock_key)
        .await
        .iter()
        .filter(|event| event.event_type == StockReserved::EVENT_TYPE)
        .map(decode::<StockReserved>)
        .collect()
}

/// The fact event the order recorded — the upstream event the operation id is
/// derived from.
async fn order_fact(store: &Journal, order_key: &str) -> OrderPlaced {
    let events = load_stream(store, order_key).await;
    let placed = events
        .iter()
        .find(|event| event.event_type == OrderPlaced::EVENT_TYPE)
        .expect("the placed order must be recorded on its own stream");
    decode(placed)
}

/// Poll the stock item's stream until the delivered reservation lands on it.
///
/// Two asynchronous steps stand between the order and that record — the saga's
/// subscription catching up, then the host's poll — and the stream is where the
/// result becomes observable, so this waits on that fact rather than sleeping
/// for a guessed duration.
async fn wait_for_first_reservation(store: &Journal, stock_key: &str) -> StockReserved {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if let Some(first) = reservations(store, stock_key).await.into_iter().next() {
            return first;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "the saga must drive a reservation into the stock item within {DELIVERY_TIMEOUT:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
