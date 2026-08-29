use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Decider, Decision};

#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example")]
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
    /// Placing the order asks nothing back: what follows from it is the saga's
    /// reservation, which is observed on the inventory's own stream.
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced { sku: cmd.sku }]).output(())
    }
}
