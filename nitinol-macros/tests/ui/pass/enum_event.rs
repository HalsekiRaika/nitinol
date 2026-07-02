//! A unit-arm enum must derive `Event`, generating the type-level `EVENT_TYPE`
//! const and a per-arm `variant()` match.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "saga.upstream")]
enum OrderEvent {
    Placed,
    Cancelled,
}

fn main() {
    let _ = OrderEvent::EVENT_TYPE;
    let _ = OrderEvent::Placed.variant();
    let _ = OrderEvent::Cancelled.variant();
}
