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
//! | System-bound spawn builder | [`SagaSpawn`] (from [`SagaSystemExt::spawn_saga`]) |
//! | Upstream subscription value | [`Subscription`] |
//! | Handle | [`SagaProxy`] |
//! | Instance-manager spawn builder | [`SagaManagerProps`] |
//! | Instance-manager handle | [`SagaManagerProxy`] |
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
//! - Symmetric spawn: [`SagaSystemExt`] extends
//!   [`nitinol_eventsource::system::EventSourceSystem`] with
//!   [`spawn_saga`](SagaSystemExt::spawn_saga), the counterpart of
//!   `EventSourceSystem::spawn_aggregate` — both event codecs and the
//!   `ProcessSystem` come from the system, and store plus start position arrive
//!   as one [`Subscription`].  [`SagaProps`] stays the entry point for the
//!   settings that builder does not take.
//! - Instance management: [`SagaManagerProps`] spawns one manager that holds a
//!   *single* upstream subscription for every instance of a saga type, so an
//!   upstream record is decoded once no matter how far the correlation fans
//!   out.  It interprets [`Saga::correlate`] at runtime: a correlation id with
//!   no resident instance is spawned and replayed on arrival, an id already
//!   resident is handed the event directly, and
//!   [`SagaManagerProps::with_instance_passivation`] stops instances that fall
//!   idle — a later event revives them from their own stream.  Owning the one
//!   cursor makes it the manager's decision whether a record is settled: it
//!   holds the cursor when the addressed instance could not settle the record,
//!   and advances past a record no instance claims so one unclaimed record
//!   cannot stall the rest.  A [`Saga::correlate`] that always answers the same
//!   id reduces the manager to the single resident saga [`SagaProps`] spawns.
//! - Deadline scheduling: call [`spawn_scheduler`] once at startup to obtain a
//!   [`SchedulerProxy`], then pass it to [`SagaProps::with_scheduler`] for a
//!   standalone saga, or to
//!   [`SagaManagerProps::with_scheduler`] so every instance the manager spawns
//!   gets the same wiring.  The saga can then emit [`SagaEffect::schedule`] /
//!   [`SagaEffect::CancelSchedule`] effects and override
//!   [`crate::Saga::on_scheduled`] to handle fired timers.  Timers are
//!   persisted in the saga's own event stream (`SagaPersisted::Schedule`) and
//!   re-registered on restart (at-least-once delivery; handlers must be idempotent).
//! - Dead-letter queue (DLQ): each saga failure kind is persisted as a
//!   `SagaPersisted::DeadLetter(`[`DeadLetterEvent`]`)` on the saga's own
//!   EventStore stream, mixed into the same envelope as domain events.
//!   `TellFailed` and `PersistFailed` exhaust staged retry before being enqueued;
//!   `HandleFailed`, `DecodeFailed`, `EndedSagaReceivedMessage`,
//!   `ScheduledFailed`, and `TellFailedHookFailed` are enqueued immediately.
//!   A subscriber catches up via
//!   [`SagaProps::with_dead_letter_subscriber`] for a standalone saga, or
//!   [`SagaManagerProps::with_dead_letter_subscriber`] to receive the dead
//!   letters of every instance a manager spawns (DurableStream-based catchup
//!   either way).  The [`EnqueuePolicy`] passed to
//!   [`SagaProps::with_enqueue_policy`] — or to
//!   [`SagaManagerProps::with_enqueue_policy`] for every instance of a manager
//!   — controls which failure kinds reach the DLQ; the default enqueues every
//!   kind.  Pull API (list / mark_processed / evict) is **not implemented** in
//!   this crate.
//! - Tell-failure compensation: when a tell exhausts its retry budget the saga
//!   is notified through [`Saga::on_tell_failed`] as soon as the failure
//!   settles, so a compensating [`SagaEffect`] runs without waiting for another
//!   upstream event.  Failures recovered by replay instead reach the saga
//!   through [`SagaContext::failed_tell_ids`].
//! - No snapshotting: a saga replays from its own event stream, and the [`Saga`]
//!   trait carries no snapshot surface.  Snapshot support is planned as a
//!   separate opt-in `SagaSnapshotable` extension trait, symmetrical to
//!   [`nitinol_eventsource::Snapshotable`].
//! - Correlation and routing are separate, and live in different places.
//!   [`Saga::correlate`] is domain knowledge — it derives a process instance's
//!   [`SagaId`] from a typed upstream event, so it belongs to the saga type and
//!   never appears in spawn wiring.  A subscribed event reaches [`Saga::handle`]
//!   only when that answer equals the instance's own id; anything else is
//!   ignored silently.  Decode failures carry no typed event to correlate on, so
//!   attributing them is routing and stays a setting on the spawn builder:
//!   [`SagaProps::with_decode_failure_route`] for a standalone saga, and
//!   [`SagaManagerProps::with_decode_failure_route`] for a manager, where the
//!   route function additionally decides *which* resident instance owns the
//!   corrupt record.
//! - Side-effect failures and persistence failures are enqueued to the DLQ
//!   after staged retry, not silently logged and discarded.

mod context;
mod dead_letter;
mod effect;
mod error;
mod id;
mod journal;
mod outbox;
mod persisted;
mod process;
mod saga;
mod scheduler;
mod system_ext;

pub use self::context::SagaContext;
pub use self::dead_letter::{
    DeadLetterEvent, EnqueueDecision, EnqueuePolicy, SagaFailure, SourceContext,
};
pub use self::effect::{SagaEffect, ScheduleSpec, TellIntent};
pub use self::id::SagaId;
pub use self::process::{
    CodecSet, CodecUnset, SagaManagerProps, SagaManagerProxy, SagaProps, SagaProxy,
    SubscriptionSet, SubscriptionUnset,
};
pub use self::saga::Saga;
pub use self::scheduler::{spawn_scheduler, ScheduleToken, SchedulerProxy, TimerName};
pub use self::system_ext::{SagaSpawn, SagaSystemExt, Subscription};
