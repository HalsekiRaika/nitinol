//! The receiving aggregate — where the idempotence lives.
//!
//! A reservation arrives as a command carrying the operation id that names it,
//! and the stock item recognises a repeat from the ids its *own stream*
//! records.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event};
use nitinol_persistence::{EventType, Family, TypeName};

use crate::codec::JsonCodec;

/// The fact a reservation was applied.
///
/// `reservation` is the operation id.  It is recorded here because the event is
/// the only durable place a later delivery of the same operation can be
/// recognised from: a set held only in the activation that applied it lasts no
/// longer than that activation does.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StockReserved {
    /// The operation this record is the application of.
    pub reservation: String,
    /// How much it reserved.
    pub quantity: u64,
}

impl Event for StockReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.idempotent"),
        TypeName::new("StockReserved"),
    );

    type Codec = JsonCodec;
}

/// Reserve `quantity` of this stock item on behalf of the operation
/// `reservation`.
///
/// `reservation` is the operation id, carried as ordinary domain data because a
/// dispatched command carries nothing else — what it does and does not carry is
/// stated on [`SagaEffect::tell`](nitinol_saga::SagaEffect::tell).  That the
/// same operation comes out with the same id every time it is constructed is the
/// sender's obligation, discharged by
/// [`ReservationSaga::reservation_id`](crate::saga::ReservationSaga::reservation_id).
///
/// `stock` is the target's own stream key.  [`Decider::decide`] never reads it;
/// it is what makes the command routable on its own, which is what lets
/// [`ReservationSaga::crash_restart_intent`](crate::saga::ReservationSaga::crash_restart_intent)
/// rebuild a reference to the target out of these very bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reserve {
    pub reservation: String,
    pub stock: String,
    pub quantity: u64,
}

/// One item of stock, and the reservations it has applied.
#[derive(Default)]
pub struct StockItem {
    /// Every reservation this item applied, in the order its own stream records
    /// them — the single owner of which operations were applied and how much
    /// they reserved, so the two cannot drift apart the way a separate running
    /// total and a separate id set could.
    reservations: Vec<StockReserved>,
}

impl StockItem {
    /// Stream key of the stock item for `sku`.
    ///
    /// This derivation is the single owner of that format: the saga addresses
    /// the item through it, and so does every reader that wants to find one.
    pub fn stream_key(sku: &str) -> String {
        format!("stock/{sku}")
    }

    /// Whether `reservation` has already been applied.
    fn already_applied(&self, reservation: &str) -> bool {
        self.reservations
            .iter()
            .any(|applied| applied.reservation == reservation)
    }
}

impl Aggregate for StockItem {
    type Event = StockReserved;

    fn apply(&mut self, event: StockReserved) {
        self.reservations.push(event);
    }
}

impl Decider<Reserve> for StockItem {
    /// The reservation asks nothing back — the saga dispatches it with `tell` —
    /// and what it did is read from this item's own stream.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<StockReserved, (), Self::Rejection> {
        if self.already_applied(&cmd.reservation) {
            // A repeat is a command that found nothing left to do, not a domain
            // rule violation, so it is accepted and states no facts.  Refusing
            // it would leave a caller unable to tell a redelivery apart from a
            // request the domain actually turned down.
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![StockReserved {
            reservation: cmd.reservation,
            quantity: cmd.quantity,
        }])
        .output(())
    }
}
