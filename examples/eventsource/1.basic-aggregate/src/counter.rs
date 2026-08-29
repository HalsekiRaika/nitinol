//! Counter aggregate — the minimal "hello world" for nitinol-eventsource.
//!
//! Demonstrates:
//! - Defining an `Event` with a stable `EventType` string
//! - Implementing `Aggregate` with a pure `apply` function
//! - Implementing `Decider<C>` to return a `Decision` — the facts a command
//!   produced together with the answer it asked for
//! - Implementing `Query<M>` for read-only queries

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Query};

// Events

/// The single event type for the Counter aggregate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[event(family = "basic_aggregate.counter")]
pub struct Incremented;

// Aggregate state

/// A simple counter aggregate.
///
/// State transitions are driven entirely by `Incremented` events.
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

/// Command: increment the counter by one.
pub struct Increment;

/// Query: return the current counter value.
pub struct GetCount;

impl Decider<Increment> for Counter {
    /// The counter's value once this command has been carried out.  `ask`
    /// answers with this, not with the events: the fact is the aggregate's own
    /// record, the answer is what the caller asked for.
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
