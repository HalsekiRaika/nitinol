//! The aggregate that owns the decision the reservation follows from.
//!
//! Placing an order is one decision on one stream; the reservation it owes the
//! stock item is a consequence of that decision, carried downstream of the fact
//! event rather than written inside the same append.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
use nitinol_persistence::{EventType, Family, TypeName};

use crate::codec::JsonCodec;

/// The fact event: "this order was placed for this much of this stock item".
///
/// Every field is here because a consumer downstream needs it and cannot reach
/// back to where the record was read from:
/// [`Saga::correlate`](nitinol_saga::Saga::correlate) and
/// [`Saga::handle`](nitinol_saga::Saga::handle) are handed the decoded event and
/// nothing else — not the stream key, not the sequence.  `order_id` names the
/// process instance *and* is the domain fact the operation id is derived from
/// (see [`ReservationSaga::reservation_id`](crate::saga::ReservationSaga::reservation_id));
/// `sku` names the stock item the reservation is addressed to; `quantity` is the
/// reservation's size.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderPlaced {
    /// Stream key of the order that was placed.
    pub order_id: String,
    /// The stock item this order reserves against.
    pub sku: String,
    /// How much of it the order reserves.
    pub quantity: u64,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.idempotent"), TypeName::new("OrderPlaced"));

    type Codec = JsonCodec;
}

/// The decision owner.
///
/// It keeps no state: nothing in this example asks the order a question after it
/// has been placed, and how far the reservation got is a fact of the stock
/// item's stream rather than of this one.  Mirroring it here would give the same
/// fact a second owner that a crash between the two writes could leave
/// disagreeing.
#[derive(Default)]
pub struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;

    fn apply(&mut self, _event: OrderPlaced) {}
}

/// Place this order for `quantity` of `sku`.
///
/// `order_id` is the order's own stream key.  It is on the command because the
/// placement is this stream's genesis decision: there is no earlier fact for the
/// aggregate to have learned its identity from, and a decider is handed `&self`
/// and a command and is told nothing about which stream it serves.
pub struct PlaceOrder {
    pub order_id: String,
    pub sku: String,
    pub quantity: u64,
}

impl Decider<PlaceOrder> for Order {
    /// The placement asks nothing back: what it is worth is the reservation that
    /// follows, and that becomes observable on the stock item's own stream.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced {
            order_id: cmd.order_id,
            sku: cmd.sku,
            quantity: cmd.quantity,
        }])
        .output(())
    }
}
