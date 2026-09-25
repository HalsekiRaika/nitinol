//! The same operation delivered twice reaches the receiving aggregate's stream
//! once, and the repeat is accepted rather than refused.
//!
//! When a second delivery can happen is stated on
//! [`nitinol_saga::SagaEffect::tell`].

#[path = "common/helpers.rs"]
mod common;
use common::{
    drain_stock_item, fingerprint, load_stream, recorded_reservations, requested_reservations,
    single_store, spawn_manager, spawn_system, spawn_world, stock_ref, wait_for_decisions,
    wait_for_records, DISPATCH_TIMEOUT, ORDER_QUANTITY, REDISPATCH_SETTLE, SKU,
};

use std::sync::Arc;

use nitinol_persistence::AggregateId;

use saga_idempotent_receive::order::PlaceOrder;
use saga_idempotent_receive::saga::ReservationSaga;
use saga_idempotent_receive::stock_item::{Reserve, StockItem};

/// The operation id the direct-dispatch test supplies itself.
///
/// It stands in for the one a saga would have derived: that test drives the
/// receiving aggregate without a saga, so the value it recognises duplicates by
/// has to come from the caller — which is the point of carrying the id as domain
/// data.
const DIRECT_RESERVATION: &str = "order-direct/reservation";

/// Given a stock item that has applied a reservation, When the very same command
/// arrives again, Then it is accepted and its stream is left exactly as it was.
#[tokio::test]
async fn a_repeated_reserve_is_accepted_and_appends_nothing() {
    let store = single_store();
    let system = spawn_system(Arc::clone(&store)).await;

    let stock_key = StockItem::stream_key(SKU);
    let stock = stock_ref(&system, &stock_key);
    let reserve = Reserve {
        reservation: DIRECT_RESERVATION.to_owned(),
        stock: stock_key.clone(),
        quantity: ORDER_QUANTITY,
    };

    stock
        .ask(reserve.clone())
        .await
        .expect("the first Reserve must be accepted");

    let before = fingerprint(&store, &stock_key).await;
    assert_eq!(
        before.len(),
        1,
        "the first Reserve must reach the stock item's stream as exactly one record"
    );

    // An `ask`, not a `tell`: the answer is what the acceptance of a duplicate is
    // observable as, and `tell` discards it.
    stock
        .ask(reserve)
        .await
        .expect("a duplicate Reserve must be accepted, not refused");

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "a duplicate Reserve must leave the stock item's stream unchanged"
    );
}

/// Given an order whose reservation already reached the stock item, When a second
/// manager incarnation re-processes the same fact event, Then the re-dispatched
/// command changes nothing on the stock item's stream.
///
/// The manager is stopped and spawned again over the same store; its cursor is in
/// memory, so the second incarnation replays the fact event from the start.  The
/// stock item's process stays resident across the restart, so this is redelivery
/// in its plain form — no aggregate replay involved.
#[tokio::test]
async fn a_redispatched_tell_writes_no_second_reservation() {
    let store = single_store();
    let order_id = AggregateId::new("order-redispatch");
    let stock_key = StockItem::stream_key(SKU);

    let world = spawn_world(&order_id, Arc::clone(&store)).await;

    world
        .order
        .ask(PlaceOrder {
            order_id: order_id.as_str().to_owned(),
            sku: SKU.to_owned(),
            quantity: ORDER_QUANTITY,
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    let stock_stream = wait_for_records(&store, &stock_key, 1, DISPATCH_TIMEOUT).await;
    let before = fingerprint(&store, &stock_key).await;

    let recorded = recorded_reservations(&stock_stream);
    let [reservation] = recorded.as_slice() else {
        panic!("the first dispatch must record exactly one reservation (recorded {recorded:?})")
    };

    // Stopping the manager cascade-stops its poller and its saga instances;
    // dropping the handle alone would leave them running.
    world
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    let _second_manager = spawn_manager(&world.system, &order_id).await;

    let saga_key = ReservationSaga::instance_id(order_id.as_str());
    let saga_stream = wait_for_decisions(&store, saga_key.as_str(), 2, DISPATCH_TIMEOUT).await;
    tokio::time::sleep(REDISPATCH_SETTLE).await;

    // So the assertions below cannot pass merely because the re-dispatched command
    // was still queued.
    drain_stock_item(&world.system, &stock_key, reservation).await;

    let requested = requested_reservations(&saga_stream);
    assert!(
        requested.len() >= 2,
        "the second incarnation must really have re-processed the fact event; without a \
         second decision nothing was absorbed (saga recorded {requested:?})"
    );

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "a re-dispatched tell must leave the stock item's stream unchanged"
    );
    assert_eq!(
        load_stream(&store, order_id.as_str()).await.len(),
        1,
        "re-processing must not add records to the stream that owns the decision"
    );
}
