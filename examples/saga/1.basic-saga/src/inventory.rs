//! The aggregate the saga's decision is addressed to.
//!
//! Nothing here knows a saga exists.  A delivery host reaches this aggregate
//! through the same `AggregateProxy` any other caller would resolve, so the
//! receiving side of a saga is an ordinary `Aggregate` + `Decider` pair and
//! implements no trait the saga introduced.

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Query};

use crate::codec::JsonCodec;

/// The fact a carried-out reservation leaves behind.
///
/// It records the `reservation` it was made for, which is what lets `decide`
/// below recognise a reservation it has already made.
#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example", codec = JsonCodec)]
pub struct Reserved {
    pub sku: String,
    pub reservation: String,
}

#[derive(Default)]
pub struct Inventory {
    reserved_count: u64,
    /// Every reservation this inventory has already made, folded out of its own
    /// stream — so the knowledge survives a restart rather than living only in
    /// the activation that first saw the command.
    reservations: Vec<String>,
}

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, event: Reserved) {
        self.reserved_count += 1;
        self.reservations.push(event.reservation);
    }
}

/// Reserve `sku` under the reservation id `reservation`.
///
/// A command needs neither `Clone` nor a serializer: a delivery host writes no
/// copy of one down.  What it carries is `reservation` — the operation id the
/// saga's decision named — because delivery is at-least-once and recognising a
/// repeat is the receiver's business.
pub struct Reserve {
    pub sku: String,
    pub reservation: String,
}

impl Decider<Reserve> for Inventory {
    /// The host `ask`s, so the answer is what tells it the command was carried
    /// out; this one has nothing to report beyond that, and the reservation is
    /// read afterwards with `GetReservedCount`.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        // Idempotent receive: the same reservation arriving twice is accepted
        // with nothing written.  A repeat is not an error — it is what
        // at-least-once delivery looks like from here — so answering with a
        // rejection would have the host hold a delivery that already landed.
        if self
            .reservations
            .iter()
            .any(|made| made == &cmd.reservation)
        {
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![Reserved {
            sku: cmd.sku,
            reservation: cmd.reservation,
        }])
        .output(())
    }
}

pub struct GetReservedCount;

impl Query<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetReservedCount) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}
