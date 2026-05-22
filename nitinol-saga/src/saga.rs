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
    /// Per spec, MVP only logs handle errors and continues; this type exists
    /// to keep the signature symmetrical with `Decider::Rejection` and to
    /// give the implementation a place to surface diagnostics.
    type Error: std::error::Error + Send + Sync + 'static;

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
}
