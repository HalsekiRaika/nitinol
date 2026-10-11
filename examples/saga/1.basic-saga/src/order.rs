use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision};

use crate::codec::JsonCodec;

#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example", codec = JsonCodec)]
pub struct OrderPlaced {
    pub sku: String,
}

#[derive(Default)]
pub struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;

    fn apply(&mut self, _event: OrderPlaced) {}
}

pub struct PlaceOrder {
    pub sku: String,
}

impl Decider<PlaceOrder> for Order {
    /// Placing the order asks nothing back.  What follows from it — the
    /// reservation — is decided by a saga that observes this stream and carried
    /// out on the inventory, so none of it is part of this decision and none of
    /// it is awaited here.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced { sku: cmd.sku }]).output(())
    }
}
