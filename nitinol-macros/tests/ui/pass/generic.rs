//! Generic types whose parameters satisfy the `Event` supertrait bounds must
//! derive cleanly; `split_for_impl` threads type params and where-clauses.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "generic")]
struct Wrapper<T: Clone + Send + Sync + 'static> {
    inner: T,
}

fn main() {
    let _ = <Wrapper<u32> as Event>::EVENT_TYPE;
    let _ = Wrapper { inner: 1u32 }.variant();
}
