//! An explicitly empty family (`family = ""`) is a valid, intentional value —
//! it must compile, mirroring the hand-written `Family::new("")` pattern.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "")]
struct Incremented;

fn main() {
    let _ = Incremented::EVENT_TYPE;
}
