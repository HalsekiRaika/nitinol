//! The `Saga` trait — user-facing process-manager abstraction.

use async_trait::async_trait;

use nitinol_eventsource::Event;
use nitinol_persistence::AggregateId;

use crate::context::SagaContext;
use crate::effect::SagaEffect;
use crate::id::SagaId;

/// Event-sourced process manager.
///
/// A saga reacts to events emitted by upstream aggregates and produces a
/// declarative [`SagaEffect`] describing what should happen next: persist new
/// saga events, send commands to other processes, or do nothing.
///
/// # Why `apply` is synchronous and `handle` is async
///
/// `apply` is a pure state transition driven by the saga's own events during
/// replay — it does no I/O.  Keeping it synchronous mirrors
/// [`nitinol_eventsource::Aggregate::apply`] and makes replay deterministic
/// and trivially testable without a runtime.
///
/// `handle` is async because it represents the saga's reactive entry point:
/// the implementation may await proxies, codecs, or domain services to
/// produce its effect.
///
/// # The user never sees the runtime `Process` trait
///
/// `Saga` is the only trait the user implements.  The internal
/// `SagaProcess<S: Saga>` wrapper that adapts it to `nitinol_runtime::Process`
/// is `pub(crate)` and never appears in any signature exposed by this crate.
///
/// # Where the saga's state lives
///
/// A saga's state is the implementor itself: [`Saga::apply`] mutates `&mut
/// self`, so fields on the implementing type are the whole of the state.  The
/// trait carries no associated state type.
///
/// # Snapshotting is not part of this trait
///
/// A saga replays purely from its own event stream.  When snapshotting is
/// implemented it arrives as a separate opt-in extension trait
/// (`SagaSnapshotable`, symmetrical to
/// [`nitinol_eventsource::Snapshotable`]) carrying an associated snapshot type
/// plus its capture/restore pair — so a saga that does not snapshot never sees
/// the surface, and the ability is visible in the type system rather than
/// hidden behind a default that panics.
#[async_trait]
pub trait Saga: Send + Sync + 'static {
    /// The upstream domain event the saga reacts to.
    ///
    /// This is the event type produced by the aggregate(s) the saga watches.
    type SubscribedEvent: Event;

    /// The saga's own internal events — used to replay state when the saga
    /// process restarts.
    type Event: Event;

    /// Domain-level error type produced by [`Saga::handle`].
    ///
    /// The MVP only logs handle errors and continues; this type exists
    /// to keep the signature symmetrical with `Decider::Rejection` and to
    /// give the implementation a place to surface diagnostics.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The typed payload delivered to [`Saga::on_scheduled`] when a scheduled
    /// timer fires.
    ///
    /// [`SagaEffect::schedule`](crate::SagaEffect::schedule) serializes a value
    /// of this type into the schedule's payload; on firing it is deserialized
    /// and handed back to `on_scheduled`.  A saga that does not schedule uses
    /// `type ScheduledMessage = ();`.
    type ScheduledMessage: serde::Serialize + serde::de::DeserializeOwned + Send + 'static;

    /// Derive the identity of the business process instance an upstream event
    /// belongs to — "how do I get found?".
    ///
    /// # Why this is an associated function
    ///
    /// The runtime has to decide whom an event belongs to *before* it has a
    /// saga to ask, so correlation cannot depend on instance state.  Taking no
    /// receiver states that in the type system: the answer is derived from the
    /// event alone, and the call site is `<S as Saga>::correlate(&event)`.
    ///
    /// # Why correlation, and not routing
    ///
    /// Correlation is the domain knowledge that maps an event to a process
    /// instance; routing is the runtime responsibility of carrying a message to
    /// wherever that instance lives.  This trait declares the former, so it
    /// travels with the saga type instead of leaking into every spawn site.
    /// The latter stays with the runtime — see
    /// [`SagaProps::with_decode_failure_route`](crate::SagaProps::with_decode_failure_route)
    /// and
    /// [`SagaManagerProps::with_decode_failure_route`](crate::SagaManagerProps::with_decode_failure_route)
    /// for the one case that has no typed event to correlate on.
    ///
    /// # How the answer is used
    ///
    /// A subscribed event reaches [`Saga::handle`] only when the returned
    /// [`SagaId`] equals the id this instance was spawned with.  `None`, or a
    /// `Some` naming a different instance, means the event belongs to somebody
    /// else and is ignored silently — it is not a failure and records no dead
    /// letter.
    ///
    /// There is deliberately no default: correlation has no universal rule, and
    /// a defaulted `None` would silently discard every upstream event.
    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId>;

    /// Apply one of the saga's own events to the in-memory state.
    ///
    /// Called during replay (`on_start`) for every event in the saga's event
    /// stream, and again after each [`SagaEffect::Persist`] event is appended.
    fn apply(&mut self, event: Self::Event);

    /// Reactive entry point.  Invoked for every subscribed event that
    /// [`Saga::correlate`] maps to this saga instance.
    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error>;

    /// Timer-driven entry point invoked when a scheduled message fires.
    ///
    /// Delivered at-least-once: the saga must treat `on_scheduled` idempotently.
    /// The default is a no-op returning [`SagaEffect::None`]; a saga that opts
    /// into scheduling overrides this hook and sets a non-`()`
    /// [`Saga::ScheduledMessage`].
    async fn on_scheduled(
        &mut self,
        message: Self::ScheduledMessage,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let _ = (message, ctx);
        Ok(SagaEffect::None)
    }

    /// Failure entry point invoked when a tell dispatched by this saga has
    /// exhausted its retry budget.
    ///
    /// Without this hook a saga whose upstream fell silent would never learn
    /// that its tell failed, and its compensation would never run.  The hook is
    /// therefore called the moment the failure settles, and the
    /// [`SagaEffect`] it returns is interpreted exactly like the one
    /// [`Saga::handle`] returns — so a compensating event reaches the saga's
    /// stream with no further upstream event involved.
    ///
    /// `target` is the target aggregate's stream key, and
    /// [`SagaContext::failed_tell_ids`] carries the single failed tell this
    /// invocation is about, so a saga with several outstanding tells can tell
    /// them apart.  A failure announced here is not repeated through
    /// `failed_tell_ids` on the next [`Saga::handle`] call.
    ///
    /// The hook is skipped — and the failure survives only as a dead letter —
    /// once the saga has ended and is draining its in-flight tells, because a
    /// terminated saga must produce no further effects.
    ///
    /// Delivered at-least-once across restarts: an incarnation that dies before
    /// the compensation is durable replays the failure, which then reaches
    /// [`Saga::handle`] through `failed_tell_ids`.  Compensations must
    /// therefore be idempotent.
    ///
    /// The default is a no-op returning [`SagaEffect::None`].  Returning `Err`
    /// records a dead letter instead of a compensation.
    async fn on_tell_failed(
        &mut self,
        target: AggregateId,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let _ = (target, ctx);
        Ok(SagaEffect::None)
    }
}
