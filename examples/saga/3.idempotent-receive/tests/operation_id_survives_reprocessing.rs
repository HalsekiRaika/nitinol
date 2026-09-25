//! The operation id a tell carries is the same one every time the same upstream
//! event is processed, including by an incarnation that built the command from
//! scratch.
//!
//! What re-dispatch reproduces on its own, and what it does not, is stated on
//! [`nitinol_saga::SagaEffect::tell`].

#[path = "common/helpers.rs"]
mod common;
use common::{
    drain_stock_item, fingerprint, load_stream, recorded_reservations, requested_reservations,
    single_store, spawn_world, wait_for_decisions, wait_for_records, DISPATCH_TIMEOUT,
    ORDER_QUANTITY, REDISPATCH_SETTLE, SKU,
};

use std::sync::Arc;

use nitinol_persistence::AggregateId;

use saga_idempotent_receive::order::{OrderPlaced, PlaceOrder};
use saga_idempotent_receive::saga::ReservationSaga;
use saga_idempotent_receive::stock_item::StockItem;

/// The upstream fact event an order of `order_id` records.
///
/// The test owns this construction rather than reading it back from a store: the
/// derivation below is a pure function of the event, and a fixture built by hand
/// is what lets it be exercised without a running system.
fn order_placed(order_id: &str) -> OrderPlaced {
    OrderPlaced {
        order_id: order_id.to_owned(),
        sku: SKU.to_owned(),
        quantity: ORDER_QUANTITY,
    }
}

/// Given one upstream fact event, When the operation id is derived from it twice,
/// Then both derivations agree — and two different orders do not.
///
/// The first half rejects an id minted per call (a fresh UUID in `handle`), which
/// no downstream receiver could recognise as a repeat.  The second half rejects
/// the opposite mistake — a constant, or an id derived from something every order
/// shares — which would pass the stability check while making a genuinely owed
/// reservation look like a duplicate and be silently withheld.
#[test]
fn the_operation_id_is_derived_from_the_upstream_event_alone() {
    let april = order_placed("order-april");
    let may = order_placed("order-may");

    assert_eq!(
        ReservationSaga::reservation_id(&april),
        ReservationSaga::reservation_id(&april),
        "the same upstream event must derive the same operation id"
    );
    assert_ne!(
        ReservationSaga::reservation_id(&april),
        ReservationSaga::reservation_id(&may),
        "two different orders must derive different operation ids"
    );
}

/// Given an order already reserved by one incarnation, When a second incarnation
/// re-processes the same fact event over the same durable state, Then both tells
/// carry one operation id and the business effect stays applied once.
#[tokio::test]
async fn reprocessing_the_same_upstream_event_converges_on_one_reservation() {
    let store = single_store();
    let order_id = AggregateId::new("order-reprocess");
    let stock_key = StockItem::stream_key(SKU);

    // Incarnation 1 — the reservation is applied for the first time.

    let first = spawn_world(&order_id, Arc::clone(&store)).await;

    first
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
        panic!("the first incarnation must record exactly one reservation (recorded {recorded:?})")
    };

    // Stopping the manager cascade-stops its poller and its saga instances.  The
    // stock item's process belongs to the system, not to the manager, so
    // incarnation 2 deliberately builds a new one: a replay must restore the
    // processed-id set from the store, not inherit it from memory.
    first
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Incarnation 2 — a new `ProcessSystem` over the same durable state.  Bound
    // for the rest of the test: this handle owns it, and the assertions below
    // observe the state it produces.
    let second = spawn_world(&order_id, Arc::clone(&store)).await;

    let saga_key = ReservationSaga::instance_id(order_id.as_str());
    let saga_stream = wait_for_decisions(&store, saga_key.as_str(), 2, DISPATCH_TIMEOUT).await;
    tokio::time::sleep(REDISPATCH_SETTLE).await;

    // Drains the restored stock item's mailbox before anything is claimed not to
    // have changed.
    drain_stock_item(&second.system, &stock_key, reservation).await;

    let requested = requested_reservations(&saga_stream);
    assert!(
        requested.len() >= 2,
        "the second incarnation must really have re-processed the fact event; without a \
         second decision nothing was absorbed (saga recorded {requested:?})"
    );
    assert!(
        requested.iter().all(|id| id == reservation),
        "every tell derived from the same upstream event must carry the operation id the \
         first one carried (saga asked for {requested:?}, stream recorded {reservation})"
    );

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "re-processing must leave the stock item's stream unchanged"
    );
    assert_eq!(
        load_stream(&store, order_id.as_str()).await.len(),
        1,
        "re-processing must not add records to the stream that owns the decision"
    );
}
