//! CQRS + Event Sourcing integration layer for the `nitinol` framework.
//!
//! This crate provides the core abstractions for building event-sourced
//! aggregates:
//!
//! | Trait / Type | Purpose |
//! |---|---|
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Decider<C>`](Decider) | Decides what a command means for the current state |
//! | [`Decision`] | A decider's conclusion: facts and an answer, or a refusal |
//! | [`Query<M>`](Query) | Read-only question asked of the current aggregate state |
//! | [`Event`] | Marker for domain events |
//! | [`Snapshotable`] | Opt-in snapshot support for faster replay |
//! | [`AggregateProxy`] | Identity-based reference to an aggregate; resolves a dispatch to an activation and re-resolves after one dies |
//!
//! Every trait and type in that table is defined in `nitinol-contract`, which
//! carries no async runtime, and is re-exported here unchanged: a domain crate
//! can implement them without depending on the execution machinery in this
//! crate.  What this crate adds is the *interpreter* — the activation that reads
//! a [`Decision`], persists its facts and delivers its output under the laws
//! stated in [`nitinol_contract`].
//!
//! # Getting started
//!
//! See the `examples/eventsource` directory for step-by-step examples:
//!
//! 1. `eventsource-basic-aggregate` – minimal counter aggregate
//! 2. `eventsource-multiple-deciders` – multiple commands per aggregate
//! 3. `eventsource-projection` – aggregate and projector (Catch-up and Live)
//! 4. `eventsource-snapshot` – snapshot-accelerated replay
//! 5. `eventsource-aggregate-communication` – inter-aggregate messaging, as a saga
//! 6. `eventsource-codec-switch` – custom codec

pub mod codec;
mod durable_stream;
mod system_event;

pub mod error;
mod process;
pub mod projection;
pub mod system;

// Defined in `nitinol-contract` so a runtime-free domain crate can implement
// them; re-exported here because this crate's own API is stated in terms of
// them and because these are the paths downstream code already imports.
pub use nitinol_contract::{Accepting, Aggregate, Decider, Decision, Event, Query, Snapshotable};

// Framework-managed persistent message abstraction. Hidden from docs and not
// re-exported through the umbrella crate so it stays an internal API; direct
// `nitinol-eventsource` importers can still reach it (consumer's responsibility).
#[doc(hidden)]
pub use self::error::SystemEventDecodeError;
#[doc(hidden)]
pub use self::system_event::{appending_system_event, SystemEvent};

pub use self::error::{AskError, ExecError, Retryability, TellError};
pub use self::process::{
    AggregateProps, AggregateProxy, AggregateTellTarget, CodecSet, CodecUnset,
};
pub use self::process::{SnapshotPersistor, SnapshotPersistorProxy};

#[cfg(feature = "test-helpers")]
pub mod test_helpers;

pub use self::projection::{
    EventEnvelope, EventSet, EventUnset, OriginSet, OriginUnset, ProjectionContext, Projector,
    ProjectorProps, TxProvider,
};

pub use self::durable_stream::{
    CursorSet, CursorUnset, DurableStream, DurableStreamProxy, DurableSubscription, SequenceCursor,
};
