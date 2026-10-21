//! The same operation delivered twice reaches the receiving aggregate's stream
//! once, and the repeat is accepted rather than refused.
//!
//! When a second delivery can happen, and why there is no mode that stops it,
//! is stated on [`nitinol_saga::SagaDeliveryProps`].

#[path = "common/helpers.rs"]
mod common;
use common::{
    decision_position, fingerprint, load_stream, recorded_progress, recorded_reservations,
    requested_reservations, single_store, spawn_system, spawn_world, stock_ref, wait_for_progress,
    wait_for_records, UnrecordedProgressStore, DECISION_BLOCK_LEN, DELIVERY_TIMEOUT,
    ORDER_QUANTITY, SKU,
};

use nitinol_persistence::store::InMemoryOffsetStore;
use nitinol_persistence::AggregateId;

use saga_idempotent_receive::order::PlaceOrder;
use saga_idempotent_receive::saga::ReservationSaga;
use saga_idempotent_receive::stock_item::{Reserve, StockItem};

/// The operation id the direct-dispatch test supplies itself.
///
/// It stands in for the one a saga would have written into its decision: that
/// clause drives the receiving aggregate without a saga, so the value it
/// recognises duplicates by has to come from the caller — which is the point of
/// carrying the id as domain data.
const DIRECT_RESERVATION: &str = "order-direct/reservation";

/// Given a stock item that has applied a reservation, When the very same command
/// arrives again, Then it is accepted and its stream is left exactly as it was.
#[tokio::test]
async fn a_repeated_reserve_is_accepted_and_appends_nothing() {
    let (_backend, store) = single_store();
    let system = spawn_system(store.clone()).await;

    let stock_key = StockItem::stream_key(SKU);
    let stock = stock_ref(&system, &stock_key);
    let reserve = Reserve {
        reservation: DIRECT_RESERVATION.to_owned(),
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
    // observable as, and it is also what a delivery host reads — a repeat
    // answered with a refusal would hold the host's partition on an operation
    // that is already done.
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

/// Given an order whose reservation was delivered by a host that never recorded
/// having delivered it, When a second host resumes under the same delivery id,
/// Then it carries the same operation again and changes nothing on the stock
/// item's stream.
///
/// The first host writes its progress through an offset store that refuses
/// every save, which is the durable state a host that died mid-record leaves
/// behind: the command reached its target, and nothing says so.  A second host
/// over the same journal and the same delivery id therefore reads no recorded
/// position, starts from the beginning of the saga's decisions, and re-derives
/// the very same command from the very same record — the at-least-once window
/// [`nitinol_saga::SagaDeliveryProps`] describes, reproduced rather than waited
/// for.
///
/// Which offset store a host records in is a slot of the system it runs on, so
/// the two hosts are two incarnations over the one journal.  The second one's
/// stock item is therefore a fresh activation that rebuilds its applied-id set
/// by replaying its own stream — the repeat has to be recognised from the record
/// rather than from anything the first activation still held.  The second host
/// recording progress is the barrier the assertions rest on: a host moves its
/// offset past a record only once every delivery that record implied has been
/// answered.
#[tokio::test]
async fn a_redelivered_decision_writes_no_second_reservation() {
    let (backend, store) = single_store();
    let order_id = AggregateId::new("order-redelivered");
    let stock_key = StockItem::stream_key(SKU);
    let saga_key = ReservationSaga::instance_id(order_id.as_str());

    // Where a host's progress would be kept.  The first one cannot reach it.
    let offsets = InMemoryOffsetStore::default();

    let world = spawn_world(
        &order_id,
        &backend,
        &UnrecordedProgressStore::over(&offsets),
    )
    .await;

    world
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
        panic!("the first delivery must record exactly one reservation (recorded {recorded:?})")
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
         else: a delivery host writes nothing onto the saga's own stream (journal \
         holds {:?})",
        decided
            .iter()
            .map(|event| event.event_type.to_string())
            .collect::<Vec<String>>()
    );

    assert_eq!(
        recorded_progress(&offsets).await,
        None,
        "the interrupted host must leave no progress recorded: that is what has the \
         next one carry the same deliveries again"
    );

    let position = decision_position(&store, saga_key.as_str()).await;

    // The second incarnation — same journal, same delivery id, and an offset
    // store that works. Its host resumes from what was recorded for that id,
    // which is nothing, so the decision is read and delivered a second time.
    let _resumed = spawn_world(&order_id, &backend, &offsets).await;

    wait_for_progress(
        &offsets,
        position,
        "the resumed host must deliver the decision again and record it",
    )
    .await;

    assert_eq!(
        fingerprint(&store, &stock_key).await,
        before,
        "a redelivered command must leave the stock item's stream unchanged"
    );

    let saga_stream = load_stream(&store, saga_key.as_str()).await;
    let requested = requested_reservations(&saga_stream);
    assert_eq!(
        requested.as_slice(),
        std::slice::from_ref(reservation),
        "the repeat must be a repeated delivery, not a second decision: the fact \
         event's position is on the saga's own stream, so the framework drops a \
         redelivery of it before `handle` (saga recorded {requested:?})"
    );
    assert_eq!(
        saga_stream.len(),
        DECISION_BLOCK_LEN,
        "delivery leaves the saga's own stream alone; how far a host got lives in \
         its offset (journal holds {:?})",
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
