//! `#[event(codec = ...)]` is mandatory, for the same reason `family` is.
//!
//! `Event` names the codec that writes it down, and stable Rust has no
//! associated-type defaults, so every `impl Event` must state one. A derive
//! that let the attribute be omitted could not emit the associated type at all,
//! and the user would be told `missing associated type` by the trait system
//! instead of being told, here, which attribute they left out.
//!
//! Naming a codec by hand in an `impl Event` block remains equally valid — this
//! rule binds the derive, which is the only party that must be told.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "shop.orders")]
struct Missing;

fn main() {}
