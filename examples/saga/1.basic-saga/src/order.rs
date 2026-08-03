use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::{Aggregate, Context, Decider, Effect};

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

#[async_trait]
impl Decider<PlaceOrder> for Order {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PlaceOrder,
        _ctx: &mut Context,
    ) -> Result<Effect<OrderPlaced>, Self::Rejection> {
        Ok(Effect::persist(OrderPlaced { sku: cmd.sku }))
    }
}
