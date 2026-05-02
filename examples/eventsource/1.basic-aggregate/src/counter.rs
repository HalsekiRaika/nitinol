//! Counter aggregate — the minimal "hello world" for nitinol-eventsource.
//!
//! Demonstrates:
//! - Defining an `Event` with a stable `EventType` string
//! - Implementing `Aggregate` with a pure `apply` function
//! - Implementing `Decider<C>` to produce `Effect::Persist`
//! - Implementing `Receive<Q>` for read-only queries

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive as EvtReceive};
use nitinol_persistence::EventType;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The single event type for the Counter aggregate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("BasicAggregate.Counter.Incremented");
}

// ---------------------------------------------------------------------------
// Aggregate state
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Command: increment the counter by one.
pub struct Increment;

/// Query: return the current counter value.
pub struct GetCount;

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[async_trait]
impl EvtReceive<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}
