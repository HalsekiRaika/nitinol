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
//! 3. `eventsource-projection` – aggregate + projector (Catch-up + Live)
//! 4. `eventsource-snapshot` – snapshot-accelerated replay
//! 5. `eventsource-aggregate-communication` – inter-aggregate messaging
//! 6. `eventsource-codec-switch` – custom codec
//!
//! # Design guide
//!
//! For the rationale behind key design decisions — why `apply` is synchronous,
//! why there is no Reply type, how `Effect::Side` works, etc. — see
//! `nitinol-eventsource/docs/DESIGN.md`.

mod aggregate;
pub mod codec;
mod context;
mod decider;
mod effect;
mod event;
mod receive;

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

pub use self::process::{AggregateProps, AggregateProxy};
pub use self::process::{EventPersistor, EventPersistorRef, SnapshotPersistor, SnapshotPersistorRef};
pub use self::error::{AskError, ExecError, PersistorUnreachableKind, TellError};

pub use self::projection::{EventEnvelope, ProjectionContext, Projector, ProjectorProps};
