//! Counter aggregate for the codec-switch example.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive as EvtReceive};
use nitinol_persistence::EventType;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("CodecSwitch.Counter.Incremented");
}

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
