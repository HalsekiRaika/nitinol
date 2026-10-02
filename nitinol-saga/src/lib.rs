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
//! - Consumed inputs are de-duplicated by the framework.  Most positions are
//!   written onto the saga's own stream by the append that closes out the
//!   `handle` decision the input produced — the last one, when one `handle`
//!   call reaches the store as more than one atomic append — so the position
//!   is never durable before every append it depends on is.  A record that
//!   fails to decode, arrives while an earlier correlated record is draining
//!   to a dead letter, or gets an `Err` from `handle`, is instead settled by
//!   a dead-letter append carrying the same position — these inputs are
//!   consumed without ever producing a `handle` decision.  Either way, replay
//!   rebuilds those positions from the stream alone, a redelivered record at
//!   or below one is dropped before [`Saga::handle`] runs again, and a stream
//!   subscription resumes past it instead of re-reading the upstream history.
//!   An effect that writes nothing else still commits a consumed-position-only
//!   append, so a no-op decision is not redelivered either; what the guarantee
//!   does *not* cover — an append that never landed, and the entry points with
//!   no input to consume — is stated on [`Saga::handle`].  Positions are held
//!   per source stream, so one upstream stream's progress never covers a
//!   record of another.
//! - Symmetric spawn: [`SagaSystemExt`] extends
//!   [`nitinol_eventsource::system::EventSourceSystem`] with
//!   [`spawn_saga`](SagaSystemExt::spawn_saga), the counterpart of
//!   `EventSourceSystem::spawn_aggregate` — the `ProcessSystem` comes from the
//!   system, store plus start position arrive as one [`Subscription`], and the
//!   formats are settled already by the event types themselves.  [`SagaProps`]
//!   stays the entry point for the settings that builder does not take.
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
//! - Deadline scheduling: ask the runtime for the resident timer core —
//!   `system.spawn_resident::<`[`SchedulerProcess`]`>().await.into()` — and pass
//!   the resulting [`SchedulerProxy`] to [`SagaProps::with_scheduler`] for a
//!   standalone saga, or to
//!   [`SagaManagerProps::with_scheduler`] so every instance the manager spawns
//!   gets the same wiring.  The ask is keyed on the type, so however many
//!   places make it, one scheduler exists and they all name it.  The saga can
//!   then emit [`SagaEffect::schedule`] / [`SagaEffect::CancelSchedule`]
//!   effects and override [`crate::Saga::on_scheduled`] to handle fired
//!   timers.  Deadlines are persisted in the saga's own event stream
//!   (`SagaPersisted::Schedule`), which is where they durably live: the
//!   scheduler restores nothing of its own after a restart, and an unfired
//!   deadline is re-armed when the instance that set it is next activated
//!   (at-least-once delivery; handlers must be idempotent).
//! - Dead-letter queue (DLQ): each saga failure kind is persisted as a
//!   [`DeadLetterEvent`] on the saga's own EventStore stream, mixed into the
//!   same envelope as domain events.
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
//!   kind.  That subscriber is the *push* path: an observability signal,
//!   delivered whether or not anyone has acted on it.
//! - DLQ pull API: [`DeadLetterQueue`] is the operator-facing recovery
//!   counterpart of that push path.  [`list`](DeadLetterQueue::list) returns the
//!   dead letters nobody has settled — ordered by their own stream sequence,
//!   with `from_sequence` and `limit` for paging — and
//!   [`mark_processed`](DeadLetterQueue::mark_processed) /
//!   [`evict`](DeadLetterQueue::evict) settle one by appending a
//!   `nitinol.saga.dead_letter_disposition` marker to the saga's own stream.
//!   Eviction is a *logical* delete: the marker drops the dead letter from the
//!   listing and leaves its original record in the store, which is the only
//!   kind of delete an event log has.  Reprocessing is deliberately not part of
//!   it: a [`DeadLetterEntry`] carries the recovery material (the failure's raw
//!   payload plus its [`SourceContext`] coordinates) and acting on it is the
//!   downstream application's decision.  The marker is a *sibling* declaration
//!   of `nitinol.saga.dead_letter` rather than an arm of it, so settling a
//!   dead letter changes what `list` returns and leaves the push path — which
//!   selects on the dead letter's own path — untouched.
//! - One writer per stream, dispositions included: a marker lands on the saga's
//!   *own* stream, whose next sequence a resident instance holds in memory, so
//!   an operator cannot append it beside a running saga.  Obtain the queue from
//!   [`SagaManagerProxy::dead_letter_queue`] and the write is routed to that
//!   manager — the single arbiter of every stream in its fan-out.  It appends
//!   through the instance's own mailbox when the saga is resident, and writes
//!   the marker itself when the saga is not; deciding which, and doing it,
//!   happen together inside the manager's message handling, so a lazy spawn
//!   cannot slip in between.  The operator waits for the outcome — a recovery
//!   action whose result is not reported cannot be retried on purpose.
//!   [`DeadLetterQueue::new`] still opens a queue with no arbiter, for offline
//!   triage and for a saga spawned standalone through [`SagaProps`]; there the
//!   caller is the one guaranteeing nothing else is writing that stream.
//!
//!   This is the same ownership pattern `nitinol-runtime` applies to its own
//!   dead letters — one owner, writes arriving as messages — and nothing more.
//!   The two mechanisms stay separate: runtime's is in-memory, best-effort and
//!   system-wide; this one is persisted, ordered, and has a recovery API.
//! - Reserved namespace: `nitinol` belongs to the framework, in the stream-key
//!   space and the event-type space alike.  [`SagaId`] refuses a name inside it
//!   the way [`nitinol_persistence::AggregateId`] does, and `#[derive(Event)]`
//!   refuses a `family` inside it at compile time.  One law, two enforcement
//!   points — see [`nitinol_persistence::reserved`] for the boundary rule and
//!   for what a hand-written `impl Event` is expected to honour on its own.
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
//! # What tell dispatch delivers
//!
//! A tell dispatched through the outbox is neither plainly "at-least-once" nor
//! ever exactly-once: what holds is stated per stage — intent recorded,
//! dispatch attempted, accepted for delivery, committed by the receiver,
//! business completion — and which stage a given saga reaches depends on how it
//! was wired, in particular on whether
//! [`SagaManagerProps::with_crash_restart_factory`] is registered.  That
//! contract, the replay branches that decide a pending tell's fate, the finite
//! retry budget, and the windows no wiring closes are stated once, on
//! [`SagaEffect::tell`](SagaEffect::tell).
//!
//! The framework attaches no metadata or causation envelope to a dispatched
//! command, so a receiver that must not carry an operation out twice recognises
//! the repeat from data the domain itself put on the command — the
//! idempotent-receive pattern, worked through in
//! [the idempotent-receive example](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga/3.idempotent-receive).
//!
//! # The fan-out pattern
//!
//! One decision frequently has consequences that live on other aggregates'
//! streams.  nitinol has exactly one unit of atomicity — one `append` to one
//! stream — and offers no atomic append spanning several streams; that rule,
//! and what expresses cross-stream consistency in its place, is stated as
//! OCC-3 on [`nitinol_persistence::store::EventStore::append`].  The pattern
//! below is that replacement written out; [the fan-out pattern example](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga/2.fanout-pattern)
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
//!    creation command per child with [`SagaEffect::tell`], whose delivery is
//!    staged rather than a flat at-least-once — see
//!    [`SagaEffect::tell`](SagaEffect::tell) for what each stage promises and
//!    how wiring changes it.  The upstream subscription is at-least-once, so
//!    the same fact can be *delivered* more than once; whether it is fanned out
//!    again depends on whether the batch recording the fan-out landed.  Until
//!    it does, the fact is unconsumed and a redelivery runs the fan-out again.
//!    Once it has, the fact's position is on the saga's own stream and the
//!    redelivery is dropped before [`Saga::handle`].
//! 3. **Idempotent creation.**  The child answers a repeated creation command
//!    with "already created" rather than with a second write, which is what
//!    lets a creation-only fan-out need no compensation.  A creation command
//!    does repeat even when the fact behind it is not handled twice: a tell
//!    left pending at a crash is re-dispatched from its own durable marker.
//!    Where a redelivered command reaches the store anyway, an
//!    [`nitinol_persistence::error::AppendError::SequenceConflict`] on the
//!    genesis sequence carries that same meaning — *this aggregate has already
//!    been created* — and leaves the first write intact, so a caller on the
//!    creation path may treat it as a successful response rather than a failure
//!    (OCC-2 on [`nitinol_persistence::store::EventStore::append`]).
//!
//! Two further steps belong to the pattern.  This crate carries no surface for
//! either, and [the fan-out pattern example](https://github.com/HalsekiRaika/nitinol/tree/master/examples/saga/2.fanout-pattern) implements neither:
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

mod consumed;
mod context;
mod dead_letter;
mod effect;
mod error;
mod id;
mod journal;
mod observability;
mod outbox;
mod persisted;
mod process;
mod saga;
mod scheduler;
mod system_ext;

pub use self::context::SagaContext;
pub use self::dead_letter::{
    DeadLetterEntry, DeadLetterEvent, DeadLetterQuery, DeadLetterQueue, DeadLetterQueueError,
    EnqueueDecision, EnqueuePolicy, SagaFailure, SourceContext,
};
pub use self::effect::{SagaEffect, ScheduleSpec, TellIntent};
pub use self::id::SagaId;
pub use self::process::{
    SagaManagerProps, SagaManagerProxy, SagaProps, SagaProxy, SubscriptionSet, SubscriptionUnset,
};
pub use self::saga::Saga;
pub use self::scheduler::{ScheduleToken, SchedulerProcess, SchedulerProxy, TimerName};
pub use self::system_ext::{SagaDefaultStoreExt, SagaSpawn, SagaSystemExt, Subscription};
