//! The contract between the `nitinol` runtime and downstream domain code.
//!
//! | Trait | Purpose |
//! |---|---|
//! | [`Event`] | Marker for domain events; carries their persisted identity |
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Snapshotable`] | Opt-in snapshot support for faster replay |
//!
//! All three are pure: no I/O, no `async`, and therefore no async runtime.
//! They live here rather than in `nitinol-eventsource` so that a domain-layer
//! crate can define and property-test its aggregates against the framework's
//! contract without taking on the execution machinery — or Tokio — that runs
//! them. `nitinol-eventsource` re-exports all three, so the framework-side
//! paths are unchanged.
//!
//! The execution-side abstractions (`Context`, `Effect`, `Codec`, `Decider`,
//! and the projection layer) are deliberately *not* here: they describe how the
//! runtime drives an aggregate, not what an aggregate is.

mod aggregate;
mod event;

pub use self::aggregate::{Aggregate, Snapshotable};
pub use self::event::Event;
