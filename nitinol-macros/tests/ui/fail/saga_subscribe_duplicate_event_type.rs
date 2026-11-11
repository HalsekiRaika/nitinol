//! Two declared sources may not name the same event type.
//!
//! A declared source owns a subscription and a cursor of its own, and two
//! variants over one type cannot have that: every record of the type matches
//! both, so either it is delivered twice — two decisions from one input — or one
//! of the two is silently never delivered. Neither is a behaviour to choose
//! between, so the declaration is refused where it is written.
//!
//! It is also the point at which the binding typestate stops being expressible:
//! the slot for a source is keyed by its type, so a duplicate would need two
//! slots keyed alike and `observing::<Placed>` would name neither.

use nitinol::saga::SagaSubscribe;

struct Placed;

#[derive(SagaSubscribe)]
enum Observed {
    Primary(Placed),
    Secondary(Placed),
}

fn main() {}
