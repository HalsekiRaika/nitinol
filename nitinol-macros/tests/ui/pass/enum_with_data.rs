//! Enums mixing unit, tuple, and struct arms must derive `Event`; the generated
//! `variant()` match ignores payload fields via `(..)` and `{ .. }`.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "domain")]
enum Mixed {
    Unit,
    Tuple(u32, String),
    Struct { id: u64, name: String },
}

fn main() {
    let _ = Mixed::EVENT_TYPE;
    let _ = Mixed::Unit.variant();
    let _ = Mixed::Tuple(1, String::new()).variant();
    let _ = Mixed::Struct { id: 2, name: String::new() }.variant();
}
