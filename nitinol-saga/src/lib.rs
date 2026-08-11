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
//! # Capabilities
//!
//! - Subscription-driven: the saga subscribes to an upstream [`nitinol_persistence::store::EventStore`]
//!   via a runtime child `DirectPollerProcess` (catchup + live, at-least-once delivery).
//!   The poller's lifetime is bound to the saga process; when the saga stops the runtime
//!   cascade-stops the child poller automatically.
//! - Deadline scheduling: call [`spawn_scheduler`] once at startup to obtain a
//!   [`SchedulerProxy`], then pass it to [`SagaProps::with_scheduler`].  The saga
//!   can then emit [`SagaEffect::schedule`] / [`SagaEffect::CancelSchedule`] effects
//!   and override [`crate::Saga::on_scheduled`] to handle fired timers.  Timers are
//!   persisted in the saga's own event stream (`SagaPersisted::Schedule`) and
//!   re-registered on restart (at-least-once delivery; handlers must be idempotent).
//! - Dead-letter queue (DLQ): each saga failure kind is persisted as a
//!   `SagaPersisted::DeadLetter(`[`DeadLetterEvent`]`)` on the saga's own
//!   EventStore stream, mixed into the same envelope as domain events.
//!   `TellFailed` and `PersistFailed` exhaust staged retry before being enqueued;
//!   `HandleFailed`, `DecodeFailed`, `EndedSagaReceivedMessage`, and
//!   `ScheduledFailed` are enqueued immediately.  A subscriber catches up via
//!   [`SagaProps::with_dead_letter_subscriber`] (DurableStream-based
//!   catchup).  The [`EnqueuePolicy`] returned by
//!   [`SagaProps::with_enqueue_policy`] controls which failure kinds reach the
//!   DLQ; the default enqueues every kind.  Pull API (list /
//!   mark_processed / evict) is **not implemented** in this crate.
//! - No snapshotting.
//! - Routing is a single closure `Fn(&SubscribedEvent) -> Option<SagaId>`.
//!   Decode failures (where no typed event is available) can be routed with the
//!   separate [`SagaProps::with_decode_failure_route`] closure.
//! - Side-effect failures and persistence failures are enqueued to the DLQ
//!   after staged retry, not silently logged and discarded.

mod context;
mod dead_letter;
mod effect;
mod error;
mod id;
mod outbox;
mod persisted;
mod process;
mod saga;
mod scheduler;

pub use self::context::SagaContext;
pub use self::dead_letter::{
    DeadLetterEvent, EnqueueDecision, EnqueuePolicy, SagaFailure, SourceContext,
};
pub use self::effect::{SagaEffect, ScheduleSpec, TellIntent};
pub use self::id::SagaId;
pub use self::process::{
    CodecSet, CodecUnset, SagaProps, SagaProxy, SubscriptionSet, SubscriptionUnset,
};
pub use self::saga::{Saga, SagaSnapshot};
pub use self::scheduler::{spawn_scheduler, ScheduleToken, SchedulerProxy, TimerName};
