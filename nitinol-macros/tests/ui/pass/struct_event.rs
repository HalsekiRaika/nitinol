//! A unit struct with an explicit family must derive `Event` cleanly and expose
//! the type-level `EVENT_TYPE` const plus the inherited default `variant()`.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "shop.orders")]
struct Placed;

fn main() {
    let _ = Placed::EVENT_TYPE;
    let _ = Placed.variant();
}
