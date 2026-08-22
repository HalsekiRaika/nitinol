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
//! | System-bound spawn builder | [`SagaSpawn`] (from [`SagaSystemExt::spawn_saga`] / [`SagaDefaultStoreExt::spawn_saga`]) |
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
//! - Default store: a system built with
//!   [`with_event_store`](nitinol_eventsource::system::EventSourceSystemBuilder::with_event_store)
//!   carries the store as well, and [`SagaDefaultStoreExt`] is the store-less
//!   spawn surface it unlocks — [`spawn_saga`](SagaDefaultStoreExt::spawn_saga)
//!   for a standalone saga's journal,
//!   [`subscription`](SagaDefaultStoreExt::subscription) for the upstream it
//!   polls, and [`saga_manager_props`](SagaDefaultStoreExt::saga_manager_props)
//!   for both at once on a manager.  `EventStore` is stream-keyed, so that one
//!   store holds the upstream stream and every instance's own stream side by
//!   side.  Each direction keeps its override —
//!   [`spawn_saga_with_store`](SagaDefaultStoreExt::spawn_saga_with_store),
//!   [`Subscription::stream`], [`SagaManagerProps::new`] — which takes
//!   precedence over the default.
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
//!
//! # The fan-out pattern
//!
//! One decision frequently has consequences that live on other aggregates'
//! streams.  nitinol has exactly one unit of atomicity — one `append` to one
//! stream — and offers no atomic append spanning several streams; that rule,
//! and what expresses cross-stream consistency in its place, is stated as
//! OCC-3 on [`nitinol_persistence::store::EventStore::append`].  The pattern
//! below is that replacement written out; `examples/saga/2.fanout-pattern`
//! works it through end to end on a payroll run that owes 32 employees a
//! payslip.
//!
//! 1. **One fact event.**  The aggregate that owns the decision records the
//!    whole decision as a single event on its own stream.  Several events in
//!    one stream are still one atomic write (`append(Vec)`), so a decision that
//!    fits in one stream never needs more than nitinol offers; splitting it
//!    into one event per consequence would spread it across several appends and
//!    leave a crash able to record half of it.
//! 2. **Fan-out by process manager.**  A [`Saga`] running under a
//!    [`SagaManagerProps`] manager subscribes to that fact and dispatches one
//!    creation command per child with [`SagaEffect::tell`], at-least-once,
//!    through the outbox.  The upstream subscription is at-least-once as well,
//!    so the same fact — and with it the same fan-out — can arrive more than
//!    once.
//! 3. **Idempotent creation.**  The child answers a repeated creation command
//!    with "already created" rather than with a second write, which is what
//!    lets a creation-only fan-out need no compensation: the recovery for an
//!    interrupted fan-out is to run it again.  Where a redelivered command
//!    reaches the store anyway, an
//!    [`nitinol_persistence::error::AppendError::SequenceConflict`] on the
//!    genesis sequence carries that same meaning — *this aggregate has already
//!    been created* — and leaves the first write intact, so a caller on the
//!    creation path may treat it as a successful response rather than a failure
//!    (OCC-2 on [`nitinol_persistence::store::EventStore::append`]).
//!
//! Two further steps belong to the pattern.  This crate carries no surface for
//! either, and `examples/saga/2.fanout-pattern` implements neither:
//!
//! - **Genesis by reference.**  If a child's initial state is derivable from
//!   the fact event, the child's stream need not be written at fan-out time at
//!   all: creation can be deferred to the first real command, with the fact
//!   event as the reference for what it started as.  That trades N writes for
//!   none, at the cost of a child that is not yet visible in the store.
//! - **Closing the books.**  If a reader needs "all N arrived" as a single
//!   fact, express it as one summary event written once the count is reached,
//!   rather than by making readers aggregate N streams themselves.
//!
//! ## Choosing the shape
//!
//! - *Which stream owns the decision?*  The owner is the stream whose invariant
//!   the decision belongs to — the one that would be wrong if the decision were
//!   recorded twice or not at all.  Two questions settle most cases: which
//!   single stream must reject a contradictory second decision (that stream
//!   owns it, and an answer of "several streams together" means the boundary is
//!   drawn in the wrong place, because an aggregate is the consistency
//!   boundary), and whether the consequence can be re-derived from the decision
//!   alone (if it can, it belongs downstream of a fact event rather than inside
//!   the same write).
//! - *How is the state in between modelled?*  Between the fact event and the
//!   last child there is a real intermediate state, and the pattern's answer is
//!   to *not* store it a second time.  Each child's own stream already says
//!   whether it exists, so the fan-out's progress is a query over those
//!   streams; a counter held by the saga would be a second owner of the same
//!   fact, and a crash between the two writes would leave them disagreeing with
//!   no way to tell which is right.  What the saga's own stream does hold is
//!   its decision and one outbox marker per dispatched command, committed in
//!   the same append — [`SagaEffect::combine`] folds the adjacent `Persist`
//!   branches into one batch — which is durable *intent* rather than duplicated
//!   state, and is what lets a restarted incarnation finish a dispatch it never
//!   got to.
//! - *Is the trigger event self-sufficient?*  [`Saga::correlate`] is handed the
//!   decoded event and nothing else — not the stream key it was read from, not
//!   its sequence.  An event that leaves out what its consumers need forces
//!   every consumer to reach back to where it came from, and a consumer that
//!   cannot (a projection replaying an archive, a second saga on a different
//!   subscription) cannot act on it at all.  So a trigger event should carry
//!   the deciding stream's own key together with whatever else the children are
//!   addressed by, and each dispatched command should carry its target's stream
//!   key — which is also what makes the command reconstructible from its outbox
//!   marker after a crash.
//!
//! A `SagaTestKit` — the Akka TestKit counterpart, for driving a saga through
//! these steps under test — is not provided today and may be added later.

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
pub use self::system_ext::{SagaDefaultStoreExt, SagaSpawn, SagaSystemExt, Subscription};
