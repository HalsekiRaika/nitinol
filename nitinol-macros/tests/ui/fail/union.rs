//! `#[derive(Event)]` supports structs and enums only. A union has no
//! type-name/variant mapping and must be rejected with a `syn::Error`.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "u")]
union Bad {
    a: u32,
    b: u32,
}

fn main() {}
