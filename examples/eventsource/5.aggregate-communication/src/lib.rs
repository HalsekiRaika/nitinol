//! Shared library for the `eventsource-aggregate-communication` example.
//!
//! * [`counter`] — the aggregate used as both A (which decides) and B (which is
//!   addressed).  Neither half knows a saga exists.
//! * [`saga`] — the relay: what it observes, the decision it persists, and the
//!   pure mapping from that decision to the command B is handed.
//! * `src/main.rs` — the wiring, including the delivery host that is what
//!   actually carries the command.

pub mod codec;
pub mod counter;
pub mod saga;
