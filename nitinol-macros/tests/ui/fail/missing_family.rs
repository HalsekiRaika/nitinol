//! `#[event(family = "...")]` is mandatory: proc-macro derives cannot recover a
//! module path, so a missing family attribute must be a compile error, not a
//! silent default. The family is always given by an explicit attribute.
//!
//! `codec` is supplied so that the family is the only thing missing. Its value
//! is never checked against `Codec` here: the derive refuses before it emits an
//! impl, so a placeholder keeps this fixture to one diagnostic.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(codec = ())]
struct Missing;

fn main() {}
