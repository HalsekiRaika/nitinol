//! The receiving aggregate — where the idempotence lives.
//!
//! A reservation arrives as a command carrying the operation id that names it.
//! The stock item recognises a repeat from the ids its *own stream* records, so
//! the recognition survives the activation that first applied it: a restored
//! instance rebuilds the set by replaying its events, with nothing handed over
//! in memory and nothing the framework attached to the delivery.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::{EventType, Family, TypeName};

use crate::codec::JsonCodec;

/// The fact a reservation was applied.
///
/// `reservation` is the operation id, written down as domain data — the event
/// is the only durable place it can be read back from, and reading it back is
/// what makes the next delivery of the same operation recognisable.  Keeping it
/// only in the applying activation's memory would make the pattern hold exactly
/// as long as that activation does, which is shorter than the redelivery window
/// it exists to cover.
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
/// `reservation` is the operation id and it is ordinary domain data on this
/// command: the framework attaches no envelope to a dispatched command, so a
/// receiver that needs to recognise a repeat is given the means to by whoever
/// built the command.  Its stability across separate constructions is the
/// sender's obligation — see
/// [`ReservationSaga::reservation_id`](crate::saga::ReservationSaga::reservation_id).
///
/// `stock` is the target's own stream key.  [`Decider::decide`] never reads it —
/// the only thing the decision turns on is whether this operation has been
/// applied — but it is what makes the command routable on its own, which is
/// what lets the saga's crash-restart factory rebuild a reference to the target
/// out of these very bytes (see
/// [`ReservationSaga::crash_restart_intent`](crate::saga::ReservationSaga::crash_restart_intent)).
///
/// `Clone + Serialize + Deserialize` are required by
/// [`SagaEffect::tell`](nitinol_saga::SagaEffect::tell), which serializes the
/// command into the `TellRequested` outbox marker as its crash-restart payload.
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
    /// them.
    ///
    /// This is the single owner of that fact: which operations were applied and
    /// how much they reserved are both read from here, so the two cannot drift
    /// apart the way a separate running total and a separate id set could.
    ///
    /// How far back to keep them is an application decision this example does
    /// not make.  An operation id set that only ever grows is bounded by the
    /// stream it is replayed from, and a deployment that needs it bounded
    /// smaller has to say so in the domain — by expiring reservations as a fact
    /// of their own, or by closing an order out — because dropping an id is
    /// exactly the same as forgetting that the operation happened.
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
            // The repeat of an operation already applied is a command that found
            // nothing left to do, not a domain rule violation, so it is accepted
            // and states no facts: nothing is appended and the caller is still
            // answered.  Refusing it instead would make an at-least-once
            // delivery indistinguishable from a broken one.
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![StockReserved {
            reservation: cmd.reservation,
            quantity: cmd.quantity,
        }])
        .output(())
    }
}

/// Ask a stock item which operations it considers already applied.
pub struct AppliedReservations;

impl Query<AppliedReservations> for StockItem {
    type Response = Vec<String>;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: AppliedReservations) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .reservations
            .iter()
            .map(|applied| applied.reservation.clone())
            .collect())
    }
}

/// Ask a stock item how much it has reserved — the business effect a second
/// application would double.
pub struct ReservedQuantity;

impl Query<ReservedQuantity> for StockItem {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: ReservedQuantity) -> Result<u64, Self::Error> {
        Ok(self
            .reservations
            .iter()
            .map(|applied| applied.quantity)
            .sum())
    }
}
