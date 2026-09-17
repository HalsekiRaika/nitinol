//! A unit-arm enum must derive `Event`, generating the type-level `EVENT_TYPE`
//! const, a per-arm `variant()` match, and the associated `Codec` named by the
//! attribute.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "saga.upstream", codec = OrderEventCodec)]
enum OrderEvent {
    Placed,
    Cancelled,
}

struct OrderEventCodec;

impl Codec<OrderEvent> for OrderEventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &OrderEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<OrderEvent, Self::Error> {
        Ok(OrderEvent::Placed)
    }
}

fn main() {
    let _ = OrderEvent::EVENT_TYPE;
    let _ = OrderEvent::Placed.variant();
    let _ = OrderEvent::Cancelled.variant();
}
