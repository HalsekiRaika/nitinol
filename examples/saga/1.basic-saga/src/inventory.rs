use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive as EvtReceive};
use nitinol_persistence::EventType;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reserved {
    pub sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType = EventType::from_str("saga.example.Reserved");
}

#[derive(Default)]
pub struct Inventory {
    pub reserved_count: u64,
}

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {
        self.reserved_count += 1;
    }
}

pub struct Reserve {
    pub sku: String,
    pub done_notify: Arc<Notify>,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        let done = cmd.done_notify.clone();
        let effect = Effect::persist(Reserved { sku: cmd.sku });
        done.notify_one();
        Ok(effect)
    }
}

pub struct GetReservedCount;

#[async_trait]
impl EvtReceive<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetReservedCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}
