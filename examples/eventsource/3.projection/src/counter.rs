//! Counter aggregate for the projection example.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Context, Decider, Effect};

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
