//! CQRS + Event Sourcing integration layer for the `nitinol` framework.
//!
//! This crate provides the core abstractions for building event-sourced
//! aggregates:
//!
//! | Trait / Type | Purpose |
//! |---|---|
//! | [`Aggregate`] | Domain state holder; evolves state by applying events |
//! | [`Decider<C>`](Decider) | Maps a command to an [`Effect`] (Persist / Apply / Side / tell / publish) |
//! | [`Receive<M>`](Receive) | Read-only query against the current aggregate state |
//! | [`Event`] | Marker for domain events |
//! | [`Snapshotable`] | Opt-in snapshot support for faster replay |
//! | [`Context`] | Runtime identity and sequence number |
//! | [`Effect`] | Algebraic effect ADT returned by `Decider::decide` |
//! | [`AggregateProxy`] | Handle for sending commands to a running aggregate |
//!
//! # Getting started
//!
//! See the `examples/eventsource` directory for step-by-step examples:
//!
//! 1. `eventsource-basic-aggregate` – minimal counter aggregate
//! 2. `eventsource-multiple-deciders` – multiple commands per aggregate
//! 3. `eventsource-projection` – aggregate and projector (Catch-up and Live)
//! 4. `eventsource-snapshot` – snapshot-accelerated replay
//! 5. `eventsource-aggregate-communication` – inter-aggregate messaging
//! 6. `eventsource-codec-switch` – custom codec

mod aggregate;
pub mod codec;
mod context;
mod decider;
mod durable_stream;
mod effect;
mod event;
mod receive;
mod system_event;

pub mod error;
mod process;
pub mod projection;
pub mod system;

pub use self::aggregate::{Aggregate, Snapshotable};
pub use self::context::Context;
pub use self::decider::Decider;
pub use self::effect::{Effect, SideEffect, SideEffectError};
pub use self::event::Event;
pub use self::receive::Receive;

// Framework-managed persistent message abstraction. Hidden from docs and not
// re-exported through the umbrella crate so it stays an internal API; direct
// `nitinol-eventsource` importers can still reach it (consumer's responsibility).
#[doc(hidden)]
pub use self::error::SystemEventDecodeError;
#[doc(hidden)]
pub use self::system_event::{appending_system_event, SystemEvent};

pub use self::error::{AskError, ExecError, TellError};
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
