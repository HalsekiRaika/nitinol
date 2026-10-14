//! The operation id a command carries is the same one every time that operation
//! is asked for — including when the only thing the asking incarnation has is
//! the saga's own decision record, read back out of the store.
//!
//! What a delivery host reproduces on its own, and what it does not, is stated
//! on [`nitinol_saga::SagaDeliveryProps`].

#[path = "common/helpers.rs"]
mod common;
use common::{
    decision_position, fingerprint, load_stream, recorded_progress, recorded_reservations,
    requested_reservations, single_store, spawn_delivery_host, spawn_world, wait_for_progress,
    wait_for_records, UnrecordedProgressStore, DECISION_BLOCK_LEN, DELIVERY_TIMEOUT,
    ORDER_QUANTITY, SKU,
};

use nitinol_persistence::store::InMemoryOffsetStore;
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
/// no downstream receiver could recognise as a repeat: deciding is not
/// exactly-once either — a decision can be durable while the position of its
/// input is not — so the same order can be decided from twice, and two minted
/// ids would be two operations the stock item applies in turn.  The second half
/// rejects the opposite mistake — a constant, or an id derived from something
/// every order shares — which would pass the stability check while making a
/// genuinely owed reservation look like a duplicate and be silently withheld.
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

/// Given an order already reserved by a host that never recorded having
/// delivered it, When a second incarnation re-derives that command from the
/// saga's own decision record alone, Then the reservation stays applied once.
///
/// The second incarnation is a new `ProcessSystem`, so it holds nothing from the
/// first: no saga instance of the first's, no stock-item activation of the
/// first's, and nothing of the command that was delivered — a delivery host
/// writes no copy of a command down.  The only thing it can build the command
/// from is the decision record the saga persisted, through the pure
/// [`Saga::dispatch`](nitinol_saga::Saga::dispatch) mapping, which is handed that
/// record and nothing else.  So the operation id surviving means exactly one
/// thing: it was written into the decision.
///
/// Both halves of the pattern are then under test at once — the id is re-read
/// off the saga's record, and the stock item, re-activated in the new system,
/// has to restore its applied-id set by replaying its own stream rather than
/// inherit it from the activation that applied it.  A command re-derived with
/// any other id, or a receiver that forgot, would reserve a second time.
#[tokio::test]
async fn a_decision_reprocessed_by_a_new_host_converges_on_one_reservation() {
    let (backend, store) = single_store();
    let order_id = AggregateId::new("order-reprocessed");
    let stock_key = StockItem::stream_key(SKU);
    let saga_key = ReservationSaga::instance_id(order_id.as_str());

    // Where a host's progress would be kept. The first incarnation cannot reach
    // it; the second reads it directly and finds nothing.
    let offsets = InMemoryOffsetStore::default();

    // Incarnation 1 — the reservation is applied, and the host's progress is
    // never recorded, so the operation stays owed as far as any later host can
    // tell.

    let first = spawn_world(&order_id, store.clone()).await;
    let _interrupted_host = spawn_delivery_host(
        &first.system,
        &backend,
        &UnrecordedProgressStore::over(&offsets),
    )
    .await;

    first
        .order
        .ask(PlaceOrder {
            order_id: order_id.as_str().to_owned(),
            sku: SKU.to_owned(),
            quantity: ORDER_QUANTITY,
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    let stock_stream = wait_for_records(&store, &stock_key, 1, DELIVERY_TIMEOUT).await;
    let before = fingerprint(&store, &stock_key).await;

    let recorded = recorded_reservations(&stock_stream);
    let [reservation] = recorded.as_slice() else {
        panic!("the first incarnation must record exactly one reservation (recorded {recorded:?})")
    };

    let decided = wait_for_records(
        &store,
        saga_key.as_str(),
        DECISION_BLOCK_LEN,
        DELIVERY_TIMEOUT,
    )
    .await;
    assert_eq!(
        decided.len(),
        DECISION_BLOCK_LEN,
        "one decision and the position of the fact it was decided from, and nothing \
         else (journal holds {:?})",
        decided
            .iter()
            .map(|event| event.event_type.to_string())
            .collect::<Vec<String>>()
    );
    assert_eq!(
        recorded_progress(&offsets).await,
        None,
        "the interrupted incarnation must leave no progress recorded"
    );

    let position = decision_position(&store, saga_key.as_str()).await;

    // Stopping the manager cascade-stops its poller and its saga instances.
    first
        .manager
        .stop()
        .await
        .expect("stopping the manager must succeed");

    // Incarnation 2 — a new `ProcessSystem` over the same durable state. Its
    // manager re-delivers the order's fact event, which the replayed saga
    // instance drops; its delivery host reads the decision that instance already
    // wrote and carries it again. Bound for the rest of the test: this handle
    // owns the system the assertions observe.
    let second = spawn_world(&order_id, store.clone()).await;
    let _resumed_host = spawn_delivery_host(&second.system, &backend, &offsets).await;

    wait_for_progress(
        &offsets,
        position,
        "the new host must re-derive the command from the decision and record it",
    )
    .await;

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "a command re-derived from the saga's own decision must leave the stock \
         item's stream unchanged"
    );

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    let requested = requested_reservations(&saga_stream);
    assert_eq!(
        requested.as_slice(),
        std::slice::from_ref(reservation),
        "the repeat must come from reprocessing the decision, not from a second \
         decision: the fact event's position is on the saga's own stream, so it \
         never reaches `handle` again (saga recorded {requested:?})"
    );
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN,
        "reprocessing a decision writes nothing onto the stream it was read from \
         (journal holds {:?})",
        saga_stream
            .iter()
            .map(|event| event.event_type.to_string())
            .collect::<Vec<String>>()
    );

    assert_eq!(
        load_stream(&store, order_id.as_str()).await.len(),
        1,
        "a redelivery must not add records to the stream that owns the decision"
    );
}
