//! Event-sourced process manager (Saga) for the `nitinol` framework.
//!
//! This crate is the Layer-2 (temporal coordination) counterpart of the
//! aggregate command/decide loop in `nitinol-eventsource`.  A saga subscribes
//! to upstream events, replays its own event stream to reconstruct state, and
//! emits [`SagaEffect`]s that the runtime interprets to persist the saga's own
//! decisions and to set or cancel its own deadlines.  What a decision means for
//! everybody else is the separate pure mapping [`Saga::dispatch`] answers, and
//! carrying it out belongs to a delivery host ([`SagaDeliveryProps`]).
//!
//! # Mental model
//!
//! | Concept | Type |
//! |---|---|
//! | User trait | [`Saga`] |
//! | Reactive context | [`SagaContext`] |
//! | Effect ADT | [`SagaEffect`] |
//! | Identifier | [`SagaId`] |
//! | Spawn verb | [`SagaSystemExt::host`] |
//! | Spawn builder | [`SagaManagerProps`] (what [`host`](SagaSystemExt::host) hands back, and what names a journal of its own) |
//! | Upstream subscription value | [`Subscription`] |
//! | Handle | [`SagaManagerProxy`] |
//! | Delivery mapping | [`Saga::dispatch`] answering [`SagaDelivery`] |
//! | Delivery-host spawn verb | [`SagaDeliveryExt::deliver`] |
//! | Delivery-host spawn builder | [`SagaDeliveryProps`] |
//! | Delivery-host handle | [`SagaDeliveryProxy`] |
//! | Delivery-host identifier | [`SagaDeliveryId`] |
//!
//! There is one spawn builder and one handle because there is one kind of
//! saga: [`host`](SagaSystemExt::host) always starts a manager, and a saga
//! whose [`Saga::correlate`] always answers the same [`SagaId`] is that
//! manager with a single instance rather than a second mechanism.
//!
//! The runtime `Process` trait is intentionally hidden — the user only
//! implements [`Saga`].
//!
//! # Capabilities
//!
//! - Subscription-driven: each source the saga's alphabet declares is observed
//!   through its own upstream [`nitinol_persistence::store::EventStore`], via a
//!   runtime child `DirectPollerProcess` of its own (catchup + live, at-least-once
//!   delivery), with its own cursor. Every poller's lifetime is bound to the
//!   manager process that owns it; when the manager stops the runtime
//!   cascade-stops every child poller automatically.
//!   [`Subscription::with_poll_interval`] is how often one source is asked,
//!   which is a deployment's choice rather than the framework's.
//! - Consumed inputs are de-duplicated by the framework.  Most positions are
//!   written onto the saga's own stream by the append that closes out the
//!   `handle` decision the input produced — the last one, when one `handle`
//!   call reaches the store as more than one atomic append — so the position
//!   is never durable before every append it depends on is.  An input
//!   [`Saga::handle`] answered `Err` produces no decision at all and is
//!   settled by a position-only append: it was seen, it produced nothing, and
//!   that one row is all it leaves.  Either way, replay rebuilds those
//!   positions from the stream alone and a redelivered record at or below one
//!   is dropped before [`Saga::handle`] runs again.  A source's cursor is the
//!   manager's and starts where the [`Subscription`] named, because it is
//!   spoken for by no single instance's position — so the sweep from there is
//!   also what activates the instances that are not resident.  An effect
//!   that writes nothing else still commits a consumed-position-only append,
//!   so a no-op decision is not redelivered either; what the guarantee does
//!   *not* cover — an append that never landed, and the entry points with no
//!   input to consume — is stated on [`Saga::handle`].  Positions are held
//!   per declared source of each upstream stream, so one source's progress
//!   never covers a record of another.  A record that fails to decode is
//!   settled by neither path: it has no typed input to attribute to an
//!   instance, so nothing durable is written for it and a restart repeats the
//!   same report instead of resuming past it — detailed under "Correlation and
//!   routing" below.
//! - Symmetric spawn: [`SagaSystemExt`] extends
//!   [`nitinol_eventsource::system::EventSourceSystem`] with
//!   [`host`](SagaSystemExt::host), the counterpart of
//!   `EventSourceSystem::spawn_aggregate` — the `ProcessSystem` and the journal
//!   both come from the system, and the formats are settled already by the
//!   event types themselves.  What a wiring site still says is the binding of
//!   each declared source ([`observing`](SagaManagerProps::observing)) and the
//!   instances' residency
//!   ([`with_instance_passivation`](SagaManagerProps::with_instance_passivation)),
//!   which is why one verb is enough.
//! - One supplier: a system is built with a
//!   [`PersistenceProvider`](nitinol_persistence::provider::PersistenceProvider),
//!   and [`SagaSystemExt`] resolves the journal from it —
//!   [`host`](SagaSystemExt::host) for every instance's own stream, and
//!   [`subscription`](SagaSystemExt::subscription) for an upstream it polls.
//!   `EventStore` is stream-keyed, so that one store holds the upstream stream
//!   and every instance's own stream side by side.  Reaching a *different*
//!   destination is written down rather than defaulted:
//!   [`Subscription::stream`] names a source's store, and
//!   [`SagaManagerProps::new`] names a journal.
//! - Where delivery progress is kept: [`deliver`](SagaDeliveryExt::deliver)
//!   resolves the journal and the saga-offset slot from the same
//!   supplier, and exists only where that supplier's store holds the slot — so
//!   a deployment with no store for a host's progress is told while the wiring
//!   is compiled.
//! - Reaching the host from inside: a saga spawned through [`SagaSystemExt`]
//!   is handed the hosting system by
//!   [`SagaComponents::system`](crate::SagaComponents::system), so a producer
//!   that needs a reference into the host resolves it there rather than from a
//!   clone of the system captured outside the spawn.
//! - Instance management: the manager [`host`](SagaSystemExt::host) starts
//!   holds one upstream subscription per declared source, shared by every
//!   instance of a saga type, so an upstream record is decoded once no matter
//!   how far the correlation fans out.  It interprets [`Saga::correlate`] at
//!   runtime: a correlation id with no resident instance is spawned and
//!   replayed on arrival, an id already resident is handed the event directly,
//!   and [`SagaManagerProps::with_instance_passivation`] stops instances that
//!   fall idle — a later event revives them from their own stream.  Owning each
//!   source's cursor makes it the manager's decision whether a record from
//!   that source is settled: it withholds the cursor when the addressed
//!   instance could not settle the record, and advances past a record no
//!   instance claims so one unclaimed record cannot stall the rest.  A
//!   [`Saga::correlate`] that always answers the same id reduces the manager to
//!   a single resident instance, which is how a saga with one process is run —
//!   the degenerate case, not a mechanism of its own.
//! - Delivery of what a saga decided: [`Saga::dispatch`] is a pure mapping from
//!   one of the saga's own events to the commands it implies and whom each is
//!   addressed to, and [`SagaDeliveryProps`] starts the host that carries them.
//!   The host subscribes across every instance of the saga type — it names the
//!   declared event type to the store together with the position it has
//!   finished with, so no row of another type is fetched to be discarded — and
//!   delivers with `ask`, because the receiver's answer is the only thing that
//!   says a command was carried out. A record counts as finished only once
//!   every delivery it implied has landed; nothing is passed over for failing,
//!   however permanently, so what a receiver refuses is held and said out loud
//!   rather than lost. [`SagaDeliveryProps::with_partitions`] confines a hold
//!   to the instances of one partition, each with its own progress, and
//!   [`SagaDeliveryProxy::advance_offset`] is the operator's way past one.
//!   Delivery is at-least-once with no mode to choose: a repeat is recognised
//!   by the receiver, from the operation id the domain put on the command.
//!   Where that guarantee comes from, what a hold means, and which failures are
//!   the host's as opposed to the domain's are stated once, on
//!   [`SagaDeliveryProps`].
//! - A saga's own time: declaring a `#[subscribe(deadline)]` variant in the
//!   alphabet is the whole of the wiring.  The host reads that declaration at
//!   spawn and resolves the system's one resident timer core
//!   ([`SchedulerProcess`]) for the saga — the ask is keyed on the type, so
//!   however many sagas make it, one scheduler exists and they all name it.  The
//!   saga asks its producer's [`SagaComponents`] for [`Deadline<M>`](Deadline)
//!   and sets or cancels deadlines with [`Deadline::set`] / [`Deadline::cancel`],
//!   whose effects the framework interprets as *append the fact, then arm the
//!   timer*; a firing arrives in the deadline arm of [`Saga::handle`], like any
//!   other input.  There is deliberately no handle on the timer core in a saga's
//!   reach: arming a timer no stream knows about would not survive a restart, and
//!   the restart would have no way to notice it was lost.  Deadlines are
//!   persisted in the saga's own event stream (`SagaPersisted::Schedule`), which
//!   is where they durably live: the scheduler restores nothing of its own after
//!   a restart, and an unfired deadline is re-armed when the instance that set it
//!   is next activated (at-least-once delivery; handlers must be idempotent).
//!   What the journal says is still pending is readable through
//!   [`Deadline::pending`].
//! - Termination is durable and final.  Interpreting [`SagaEffect::End`] writes
//!   an `Ended` fact onto the saga's own stream and stops the process; a later
//!   spawn over that stream replays the fact and declines to wire the upstream
//!   subscription, so [`Saga::handle`] never runs again.  Anything still routed
//!   to a terminated instance — an observed record a manager's shared
//!   subscription delivers, a deadline reached after the end — is reported
//!   through `tracing` and dropped, with nothing written: a terminated saga
//!   records no further facts, and not even a position, since a record that
//!   reached no `handle` settles nothing a later incarnation could use.  It is
//!   still answered, so one ended instance never holds a shared cursor behind
//!   it.
//! - The two layers a failure belongs to: a *business* failure is a domain fact
//!   and reaches the saga as an input — the answer the addressed aggregate
//!   recorded, observed through the source declared for it, or the saga's own
//!   deadline firing when no answer came.  A *transport* failure belongs to the
//!   delivery host, which holds and reports it (see [`SagaDeliveryProps`]); the
//!   saga is not notified and there is no hook to register for one.  Nothing in
//!   between is persisted: there is no queue of type-less bytes a saga's own
//!   failures accumulate in, and an input [`Saga::handle`] refused leaves its
//!   position and a `tracing` report rather than a durable record of the
//!   refusal.
//! - Reserved namespace: `nitinol` belongs to the framework, in the stream-key
//!   space and the event-type space alike.  [`SagaId`] refuses a name inside it
//!   the way [`nitinol_persistence::AggregateId`] does, and `#[derive(Event)]`
//!   refuses a `family` inside it at compile time.  One law, two enforcement
//!   points — see [`nitinol_persistence::reserved`] for the boundary rule and
//!   for what a hand-written `impl Event` is expected to honour on its own.
//! - No snapshotting: a saga replays from its own event stream, and the [`Saga`]
//!   trait carries no snapshot surface.  Snapshot support is planned as a
//!   separate opt-in `SagaSnapshotable` extension trait, symmetrical to
//!   [`nitinol_eventsource::Snapshotable`].
//! - Correlation and routing are separate, and live in different places.
//!   [`Saga::correlate`] is domain knowledge — it derives a process instance's
//!   [`SagaId`] from a typed input, so it belongs to the saga type and never
//!   appears in spawn wiring.  An observed event reaches [`Saga::handle`] only
//!   when that answer equals the instance's own id; anything else is ignored
//!   silently.  A record whose upstream bytes do not decode has no typed input to
//!   correlate on, so there is no instance it could be attributed to: it is
//!   reported through `tracing` and skipped, its source's cursor advances past it,
//!   and nothing durable is written for it.  A decode failure is deterministic and
//!   permanent — a stalling escape hatch would stall forever — so re-reading the
//!   record after a restart repeats the report rather than a decision.
//! - A persist batch the store refuses is retried a bounded number of times and
//!   then reported.  The input it was decided from stays unconsumed — no
//!   position is written for a decision no store holds — so a later incarnation
//!   reads the record again and decides it again.  The saga itself stays alive
//!   rather than stopping, which is what lets its next input retry the same
//!   decision, [`SagaEffect::End`] included; when that redelivery comes, and
//!   how it differs from a refused input's, is stated on [`Saga::handle`].
//!
//! # What a saga's own stream holds
//!
//! Four kinds of row and no more: a domain event — the catch-all — plus the
//! three facts the framework declares for itself, `nitinol.saga.schedule`,
//! `nitinol.saga.consumed` and `nitinol.saga.lifecycle`.  A row inside the
//! reserved namespace that matches none of those three is a corrupted stream or
//! a version mismatch, and refuses the load rather than being read past: a
//! withdrawn mechanism's bookkeeping may well decode cleanly as a domain event,
//! so reading past it would fold it into the saga's state.
//!
//! This is also why 0.4 streams cannot be read by 0.5 — see the crate's
//! CHANGELOG.
//!
//! The framework attaches no metadata or causation envelope to a delivered
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
//!    [`SagaManagerProps`] manager subscribes to that fact and records its own
//!    decision — one event naming every child the fan-out owes a command.
//!    [`Saga::dispatch`] maps that decision to one creation command per child,
//!    and a [`SagaDeliveryProps`] host carries them (at-least-once; see that
//!    type for where the guarantee comes from).  The upstream subscription is
//!    at-least-once too, so the same fact can be *delivered* more than once;
//!    whether it is decided again depends on whether the batch recording the
//!    decision landed.  Until it does, the fact is unconsumed and a redelivery
//!    runs the fan-out again.  Once it has, the fact's position is on the
//!    saga's own stream and the redelivery is dropped before [`Saga::handle`].
//! 3. **Idempotent creation.**  The child answers a repeated creation command
//!    with "already created" rather than with a second write, which is what
//!    lets a creation-only fan-out need no compensation.  A creation command
//!    does repeat even when the fact behind it is not handled twice: the host's
//!    offset moves after the deliveries it covers, so a crash in between
//!    carries them again.
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
//!   no way to tell which is right.  What the saga's own stream does hold is its
//!   decision — and, where the same `handle` call also set a deadline, the
//!   schedule fact in the very same append, since [`SagaEffect::combine`] folds
//!   the adjacent `Persist` branches into one batch.  The decision is the whole
//!   of the durable intent: what follows from it is re-derived by
//!   [`Saga::dispatch`] every time the host reads it, so nothing of the
//!   dispatch is duplicated state to fall out of step.
//! - *Is the trigger event self-sufficient?*  [`Saga::correlate`] is handed the
//!   decoded event and nothing else — not the stream key it was read from, not
//!   its sequence.  An event that leaves out what its consumers need forces
//!   every consumer to reach back to where it came from, and a consumer that
//!   cannot (a projection replaying an archive, a second saga on a different
//!   subscription) cannot act on it at all.  So a trigger event should carry
//!   the deciding stream's own key together with whatever else the children are
//!   addressed by.  The same obligation falls on the saga's own decision, for
//!   the same reason: [`Saga::dispatch`] is handed one of the saga's events and
//!   nothing else, so a decision that leaves out whom it is addressed to — or
//!   the operation id its receiver recognises a repeat by — cannot be carried
//!   out by a host that keeps no saga resident.

mod components;
mod consumed;
mod context;
mod delivery;
mod effect;
mod error;
mod id;
mod journal;
mod lifecycle;
mod observability;
mod persisted;
mod process;
mod retry_policy;
mod saga;
mod scheduler;
mod subscribe;
mod subscription;
mod system_ext;

pub use self::components::{Deadline, DeadlineMessage, PendingDeadline, SagaComponents};
pub use self::context::SagaContext;
pub use self::delivery::{
    partition_of, SagaDelivery, SagaDeliveryError, SagaDeliveryId, SagaDeliveryProps,
    SagaDeliveryProxy,
};
pub use self::effect::{SagaEffect, ScheduleSpec};
pub use self::id::SagaId;
pub use self::process::{SagaManagerProps, SagaManagerProxy};
pub use self::saga::Saga;
pub use self::scheduler::{ScheduleToken, SchedulerProcess, TimerName};
pub use self::subscribe::{
    DeadlineVariant, DeclaresDeadline, HasDeadline, NoDeadline, ReadDeadline, ReadDeadlineError,
    SagaSubscribe, SourceBindings, SubscriptionSet, SubscriptionUnset,
};
pub use self::subscription::Subscription;
pub use self::system_ext::{SagaDeliveryExt, SagaSystemExt};

/// `#[derive(SagaSubscribe)]`, co-located with the [`SagaSubscribe`] trait so a
/// single `use nitinol::saga::SagaSubscribe;` brings both into scope (trait in
/// the type namespace, derive in the macro namespace — as `serde` does, and as
/// [`nitinol::contract::Event`](nitinol_eventsource::Event) already does here).
pub use nitinol_macros::SagaSubscribe;

/// What the code `#[derive(SagaSubscribe)]` writes names, and nothing a person
/// writes by hand.
///
/// The expansion declares the binding typestate of an alphabet, so it needs the
/// slot-transition traits and the resolved-source type by name.  They are
/// exported rather than hidden behind a macro-only path because the generated
/// code lives in the user's crate and has to reach them; nothing below is part
/// of the surface a saga author reads.
///
/// No stability is promised for any of it.
#[doc(hidden)]
pub use self::subscribe::{AllSourcesSubscribed, BindSource, SourceBinding, SubscribedSource};
