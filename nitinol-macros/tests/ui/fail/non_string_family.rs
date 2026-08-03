//! `family` must be a string literal. A non-string value (here an integer
//! literal) must be rejected with a `syn::Error`, not coerced or ignored.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = 42)]
struct Bad;

fn main() {}
