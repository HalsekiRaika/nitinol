//! The reserved-namespace check is bounded by path segments, exactly as
//! `MaterializedPath::is_within` is. A family that merely starts with the same
//! letters as the reserved root is an ordinary user family and must compile —
//! a `starts_with("nitinol")` check would confiscate it.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "nitinolx")]
struct Adjacent;

fn main() {
    let _ = Adjacent::EVENT_TYPE;
}
