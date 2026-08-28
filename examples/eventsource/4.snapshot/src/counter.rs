//! Snapshotable counter aggregate.
//!
//! Implements both `Aggregate` and `Snapshotable`.  The snapshot value is the
//! raw `u64` counter — trivially captured and restored.

use async_trait::async_trait;

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Query, Snapshotable};

#[derive(Clone, Debug, PartialEq, Event)]
#[event(family = "snapshot.counter")]
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

impl Snapshotable for Counter {
    type Snapshot = u64;

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

pub struct Increment;
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

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}
