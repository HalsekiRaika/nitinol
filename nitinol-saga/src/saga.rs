//! The `Saga` trait — user-facing process-manager abstraction.

use async_trait::async_trait;

use nitinol_eventsource::Event;

use crate::context::SagaContext;
use crate::effect::SagaEffect;

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
#[async_trait]
pub trait Saga: Send + Sync + 'static {
    /// The upstream domain event the saga reacts to.
    ///
    /// This is the event type produced by the aggregate(s) the saga watches.
    type SubscribedEvent: Event;

    /// The saga's own internal events — used to replay state when the saga
    /// process restarts.
    type Event: Event;

    /// The saga's domain state.  No constraints are imposed here; if the saga
    /// needs an aggregated view, place fields directly on the implementor.
    type State: Send + Sync + 'static;

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

    /// Apply one of the saga's own events to the in-memory state.
    ///
    /// Called during replay (`on_start`) for every event in the saga's event
    /// stream, and again after each [`SagaEffect::Persist`] event is appended.
    fn apply(&mut self, event: Self::Event);

    /// Reactive entry point.  Invoked for every subscribed event whose
    /// routing function maps to this saga instance.
    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error>;

    /// Capture a snapshot of the saga's state.
    ///
    /// The MVP takes no snapshots, so the default returns `None`.  A future
    /// snapshotting implementation overrides this to return a
    /// [`SagaSnapshot`]; until then a saga replays purely from its event
    /// stream.
    fn snapshot(&self) -> Option<SagaSnapshot> {
        None
    }

    /// Reconstruct the saga from a previously captured [`SagaSnapshot`].
    ///
    /// This is a stub: with no snapshotting in the MVP there is no way to
    /// obtain a `SagaSnapshot`, so the default panics.  Implementors that
    /// override [`Saga::snapshot`] must override this as its inverse.
    fn from_snapshot(snapshot: SagaSnapshot) -> Self
    where
        Self: Sized,
    {
        let _ = snapshot;
        unimplemented!(
            "Saga::from_snapshot is a stub; override it together with Saga::snapshot \
             to restore a saga from a captured snapshot"
        )
    }

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
}

/// Opaque handle to a captured saga snapshot.
///
/// Snapshotting is not implemented in this MVP; this type is the trait-level
/// placeholder referenced by [`Saga::snapshot`] and [`Saga::from_snapshot`].
/// It is `#[non_exhaustive]` so it cannot be constructed outside this crate —
/// a future issue gives it real fields.
#[non_exhaustive]
pub struct SagaSnapshot {}
