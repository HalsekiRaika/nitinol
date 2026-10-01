//! The operation id a tell carries is the same one every time that operation is
//! asked for, including when the incarnation asking has nothing but the bytes a
//! pending marker kept for it.
//!
//! What re-dispatch reproduces on its own, and what it does not, is stated on
//! [`nitinol_saga::SagaEffect::tell`].

#[path = "common/helpers.rs"]
mod common;
use common::{
    drain_stock_item, fingerprint, load_stream, recorded_reservations, requested_reservations,
    single_store, spawn_world, wait_for_records, UnsettledDispatchStore, DECISION_BLOCK_LEN,
    DISPATCH_TIMEOUT, ORDER_QUANTITY, SKU,
};

use std::sync::Arc;

use nitinol_persistence::store::EventStore;
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

/// Given an order already reserved by one incarnation whose dispatch was never
/// recorded as settled, When a second incarnation rebuilds that command from the
/// marker's bytes and dispatches it again, Then the reservation stays applied
/// once.
///
/// The second incarnation is a new `ProcessSystem`, so it holds no intent from the
/// first: the only thing it can rebuild the command from is the crash-restart
/// payload the pending marker kept, and the stock item that absorbs the repeat has
/// to restore its applied-id set from its own stream rather than inherit it from
/// the activation that applied it.  A command rebuilt with any other id than the
/// one on that stream would be applied a second time, which is what makes this the
/// end-to-end reading of the operation id the derivation above fixes.
#[tokio::test]
async fn a_redispatch_rebuilt_from_the_marker_converges_on_one_reservation() {
    let store = single_store();
    let order_id = AggregateId::new("order-redispatch-rebuilt");
    let stock_key = StockItem::stream_key(SKU);
    let saga_key = ReservationSaga::instance_id(order_id.as_str());

    // Incarnation 1 — the reservation is applied, and the store refuses to record
    // that its dispatch settled, so the marker for it stays pending.

    let interrupted = UnsettledDispatchStore::new(Arc::clone(&store), saga_key.as_str().to_owned());

    let first = spawn_world(&order_id, Arc::clone(&interrupted) as Arc<dyn EventStore>).await;

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

    let unsettled = wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN,
        DISPATCH_TIMEOUT,
    )
    .await;
    assert_eq!(
        unsettled.len(),
        DECISION_BLOCK_LEN,
        "the interrupted incarnation must leave its dispatch unsettled: the decision \
         block alone, with no terminal marker after it (journal holds {} records)",
        unsettled.len()
    );

    // Stopping the manager cascade-stops its poller and its saga instances.  The
    // stock item's process belongs to the system, not to the manager, so
    // incarnation 2 deliberately builds a new one: a replay must restore the
    // processed-id set from the store, not inherit it from memory.
    first
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Incarnation 2 — a new `ProcessSystem` over the same durable state, reading
    // and writing the store directly: the incarnation that could not record its
    // progress is gone.  Bound for the rest of the test: this handle owns it, and
    // the assertions below observe the state it produces.
    let second = spawn_world(&order_id, Arc::clone(&store)).await;

    // The terminal marker the interrupted incarnation never wrote is what says the
    // pending tell was rebuilt and dispatched again, and it is written after the
    // command was handed to the stock item.
    wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN + 1,
        DISPATCH_TIMEOUT,
    )
    .await;

    // Drains the restored stock item's mailbox before anything is claimed not to
    // have changed.
    drain_stock_item(&second.system, &stock_key, reservation).await;

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN + 1,
        "the rebuilt command must have reached the stock item and settled as that one \
         marker: a dispatch that could not be delivered settles as a failure with a \
         dead letter beside it (journal holds {:?})",
        saga_stream
            .iter()
            .map(|event| event.event_type.to_string())
            .collect::<Vec<String>>()
    );

    let requested = requested_reservations(&saga_stream);
    assert_eq!(
        requested.as_slice(),
        std::slice::from_ref(reservation),
        "the re-dispatch must come from the pending marker, not from a second decision: \
         the fact event's position is on the saga's own stream, so it never reaches \
         `handle` again (saga recorded {requested:?})"
    );

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "a command rebuilt from the marker's bytes must leave the stock item's stream \
         unchanged"
    );
    assert_eq!(
        load_stream(&store, order_id.as_str()).await.len(),
        1,
        "a re-dispatch must not add records to the stream that owns the decision"
    );
}
