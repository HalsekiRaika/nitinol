//! Counter aggregate used as both A and B in the communication example.
//!
//! Note what is *not* here: nothing about the relay, and nothing the saga
//! introduced.  A delivery host reaches counter B through the same
//! `AggregateProxy` the example's `main` resolves for its own queries, so the
//! receiving side of aggregate-to-aggregate communication is an ordinary
//! `Aggregate` + `Decider` pair.

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Never, Query};

use crate::codec::JsonCodec;

/// The counter went up by one, on behalf of the operation named.
///
/// Recording the operation is what lets `decide` below recognise an increment
/// it has already carried out, across a restart as well as within one
/// activation — the knowledge is folded out of the counter's own stream rather
/// than kept beside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.counter", codec = JsonCodec)]
pub struct Incremented {
    pub operation: String,
}

#[derive(Default)]
pub struct Counter {
    value: u64,
    applied: Vec<String>,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, event: Incremented) {
        self.value += 1;
        self.applied.push(event.operation);
    }
}

// Commands

/// Increment the counter by one, on behalf of `operation`.
///
/// The command needs neither `Clone` nor a serializer: nothing writes a copy of
/// it down — a saga cannot hold a command at all, and a delivery host builds
/// each one from the decision it is carrying.  What it does carry is
/// `operation`, the id by which a repeat is recognised, because the relay below
/// is delivered at-least-once and the framework puts no envelope of its own on
/// a delivered command.
pub struct Increment {
    pub operation: String,
}

/// Read-only query.
pub struct GetCount;

// Decider / Query impls

impl Decider<Increment> for Counter {
    /// The counter's value once this command has been carried out.  The command
    /// asks a question — "how many now?" — so the decision answers it rather
    /// than handing back the fact it produced.
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        // Idempotent receive: an operation already carried out is accepted with
        // nothing written, and answered with the value it left behind.  A
        // repeat is what at-least-once delivery looks like from here, not an
        // error — rejecting it would have the delivery host hold a command that
        // has already landed.
        if self.applied.iter().any(|done| done == &cmd.operation) {
            return Decision::persist(Vec::new()).output(self.value);
        }
        Decision::persist(vec![Incremented {
            operation: cmd.operation,
        }])
        .output(self.value + 1)
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}
