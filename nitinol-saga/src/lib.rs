//! Event-sourced process manager (Saga) for the `nitinol` framework.
//!
//! This crate is the Layer-2 (temporal coordination) counterpart of the
//! aggregate command/decide loop in `nitinol-eventsource`.  A saga subscribes
//! to upstream events, replays its own event stream to reconstruct state, and
//! emits [`SagaEffect`]s that the runtime interprets to persist saga events
//! or send commands to other processes.
//!
//! # Mental model
//!
//! | Concept | Type |
//! |---|---|
//! | User trait | [`Saga`] |
//! | Reactive context | [`SagaContext`] |
//! | Effect ADT | [`SagaEffect`] |
//! | Identifier | [`SagaId`] |
//! | Spawn builder | [`SagaProps`] |
//! | Handle | [`SagaProxy`] |
//!
//! The runtime `Process` trait is intentionally hidden — the user only
//! implements [`Saga`].
//!
//! # MVP scope
//!
//! - Subscription-driven: the saga subscribes to an upstream [`nitinol_persistence::store::EventStore`]
//!   via a runtime child `DirectPollerProcess` (catchup + live, at-least-once delivery).
//!   The poller's lifetime is bound to the saga process; when the saga stops the runtime
//!   cascade-stops the child poller automatically.
//! - No scheduler, no compensation, no snapshotting.
//! - Routing is a single closure `Fn(&SubscribedEvent) -> Option<SagaId>`.
//! - Side-effect failures and persistence failures are logged, not
//!   propagated (consistent with `Effect::Side` in `nitinol-eventsource`).

mod context;
mod effect;
mod error;
mod id;
mod outbox;
mod process;
mod saga;

pub use self::context::SagaContext;
pub use self::effect::{SagaEffect, Schedule, TellIntent};
pub use self::id::SagaId;
pub use self::process::{
    CodecSet, CodecUnset, SagaProps, SagaProxy, SubscriptionSet, SubscriptionUnset,
};
pub use self::saga::{Saga, SagaSnapshot, ScheduledMessage};
