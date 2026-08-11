//! `#[event(family = "...")]` is mandatory: proc-macro derives cannot recover a
//! module path, so a missing family attribute must be a compile error, not a
//! silent default. The family is always given by an explicit attribute.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
struct Missing;

fn main() {}
