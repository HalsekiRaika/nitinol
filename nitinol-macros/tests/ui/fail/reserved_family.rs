//! `nitinol` is the framework's reserved namespace. It spans both the stream-key
//! space (enforced when an id is constructed) and the event-type space, where
//! the family is a compile-time literal — so the derive rejects it outright
//! rather than letting a user event impersonate a framework record at run time.

use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "nitinol.saga")]
struct Bad;

fn main() {}
