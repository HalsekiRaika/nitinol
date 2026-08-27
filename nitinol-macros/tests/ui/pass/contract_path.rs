//! A downstream crate must be able to reach both the `Event` trait and the
//! `Event` derive through `nitinol::contract`, the path the generated code
//! names — so the derive is usable without importing `nitinol::eventsource`.

use nitinol::contract::Event;

#[derive(Clone, Event)]
#[event(family = "shop.orders")]
struct Shipped;

fn main() {
    let _ = Shipped::EVENT_TYPE;
    let _ = Shipped.variant();
}
