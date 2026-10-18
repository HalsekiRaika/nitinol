//! The receiving aggregate — where the idempotence lives.
//!
//! A reservation arrives as a command carrying the operation id that names it,
//! and the stock item recognises a repeat from the ids its *own stream*
//! records.
//!
//! Nothing in this module knows a saga exists.  The stock item is an ordinary
//! [`Aggregate`] plus a [`Decider`], reached by the same reference any caller
//! would resolve, which is what makes it addressable by a delivery host without
//! implementing anything of the host's.

use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Decider, Decision, Event, Never};
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
/// delivered command carries nothing else: the framework attaches no metadata
/// and no causation envelope to what a delivery host hands over — see
/// [`SagaDeliveryProps`](nitinol_saga::SagaDeliveryProps) — so recognising a
/// repeat is the receiver's job, done with the domain's own id or not at all.
///
/// That the same operation comes out with the same id every time it is
/// constructed is the sender's obligation, discharged by
/// [`ReservationSaga::reservation_id`](crate::saga::ReservationSaga::reservation_id)
/// for a command built afresh from the order's fact event, and by the id being
/// *written into the decision* for a command re-derived from that decision by
/// [`Saga::dispatch`](nitinol_saga::Saga::dispatch).
///
/// Nothing here names the target.  Whom a command is addressed to belongs to
/// the delivery rather than to the command — `Saga::dispatch` answers a target
/// and a command side by side — and the host needs no copy of either to be
/// written down, so this type carries neither `Clone` nor a serializer for the
/// framework's sake.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reserve {
    pub reservation: String,
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
    /// This derivation is the single owner of that format: the saga writes its
    /// answer into the decision it persists, and every reader that wants to
    /// find one computes the same thing.
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
    type Recover = Never;

    fn apply(&mut self, event: StockReserved) {
        self.reservations.push(event);
    }
}

impl Decider<Reserve> for StockItem {
    /// The reservation has nothing to report back, so the answer is the empty
    /// one; what it did is read from this item's own stream.
    ///
    /// A delivery host still `ask`s rather than tells, because an answer is the
    /// only way a refusal can be told apart from a command carried out — and a
    /// repeat must therefore answer `Ok`, or the host would hold forever on an
    /// operation that is already done.
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
