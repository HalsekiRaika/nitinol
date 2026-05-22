use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, EventEnvelope};
use nitinol_persistence::EventType;
use nitinol_runtime::process::{ProcessProxy, Stream};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::from_str("saga.example.OrderPlaced");
}

#[derive(Default)]
pub struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;

    fn apply(&mut self, _event: OrderPlaced) {}
}

pub struct PlaceOrder {
    pub sku: String,
    pub stream: ProcessProxy<Stream<EventEnvelope<OrderPlaced>>>,
}

#[async_trait]
impl Decider<PlaceOrder> for Order {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PlaceOrder,
        ctx: &mut Context,
    ) -> Result<Effect<OrderPlaced>, Self::Rejection> {
        let envelope = EventEnvelope {
            aggregate_id: ctx.aggregate_id().clone(),
            sequence: ctx.sequence() + 1,
            global_sequence: 0,
            event: OrderPlaced {
                sku: cmd.sku.clone(),
            },
        };
        Ok(Effect::persist(OrderPlaced { sku: cmd.sku }).combine(Effect::publish(cmd.stream, envelope)))
    }
}
