//! Counter aggregate used as both A and B in the communication example.

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Query};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.counter")]
pub struct Incremented;

#[derive(Default)]
pub struct Counter {
    pub value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Commands

/// Increment the counter by one.
///
/// `Clone + Serialize + Deserialize` are not needed by the aggregate itself —
/// they are what lets a saga name this command in a `SagaEffect::tell`, which
/// keeps a copy to retry with and writes it into its outbox marker so a saga
/// that crashes mid-dispatch can re-issue it after a restart.
#[derive(Clone, Serialize, Deserialize)]
pub struct Increment;

/// Read-only query.
pub struct GetCount;

// Decider / Query impls

impl Decider<Increment> for Counter {
    /// The counter's value once this command has been carried out.  The command
    /// asks a question — "how many now?" — so the decision answers it rather
    /// than handing back the fact it produced.
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}
