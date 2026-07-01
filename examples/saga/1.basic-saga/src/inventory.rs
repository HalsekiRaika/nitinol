use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive as EvtReceive};
use nitinol_persistence::{EventType, Family, TypeName};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reserved {
    pub sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType = EventType::new(Family::new("saga.example"), TypeName::new("Reserved"));
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

/// `Reserve` derives `Clone + Serialize + Deserialize` so `SagaEffect::tell`
/// can serialize it as crash-restart payload in the outbox marker.
#[derive(Clone, Serialize, Deserialize)]
pub struct Reserve {
    pub sku: String,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        Ok(Effect::persist(Reserved { sku: cmd.sku }))
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
