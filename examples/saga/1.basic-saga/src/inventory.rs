use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision, Query};

#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example")]
pub struct Reserved {
    pub sku: String,
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

impl Decider<Reserve> for Inventory {
    /// The saga dispatches this with `tell`, so there is nobody waiting for an
    /// answer; the reservation is read afterwards with `GetReservedCount`.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        Decision::persist(vec![Reserved { sku: cmd.sku }]).output(())
    }
}

pub struct GetReservedCount;

impl Query<GetReservedCount> for Inventory {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetReservedCount) -> Result<u64, Self::Error> {
        Ok(self.reserved_count)
    }
}
