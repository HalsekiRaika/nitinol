//! Counter aggregate for the projection example.

use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[event(family = "projection.counter")]
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

pub struct Increment;

impl Decider<Increment> for Counter {
    /// This example reads the count from the projected read model, not from the
    /// command, so the command asks nothing and says so once, here.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}
