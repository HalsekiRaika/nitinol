use futures_core::future::BoxFuture;

/// A declarative description of side effects produced by a `Decider::decide` call.
///
/// `Effect<E>` is a Monoid: `None` is the identity element and `combine` is the
/// associative binary operation.  The `Sequence` variant accumulates leaves in a
/// flat list; 'combine' introduces no nesting.
pub enum Effect<E> {
    /// No-op — the identity element of the Monoid.
    None,
    /// Persist the given events to the `EventStore` and then apply them.
    Persist(Vec<E>),
    /// Apply the given events to the aggregate without persisting them.
    ///
    /// Intended for tests and transient state changes (e.g., UI previews). For
    /// regular CQRS and Event Sourcing flows use [`Effect::Persist`] so the events
    /// reach the `EventStore` and survive replay.
    Apply(Vec<E>),
    /// Execute an arbitrary side effect (fire-and-forget in Phase 2.4+).
    Side(Box<dyn SideEffect>),
    /// An ordered collection of effects to execute sequentially.
    Sequence(Vec<Effect<E>>),
}

/// A type-safe, object-safe side effect that can be executed asynchronously.
///
/// Implementors wrap a concrete operation (e.g. `tell` a process, `publish` to a
/// stream) so that the `Effect` ADT remains free of generic parameters tied to
/// the specific operation.
pub trait SideEffect: Send + Sync + 'static {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>>;
}

/// Error produced when a `SideEffect` fails to execute.
#[derive(Debug, thiserror::Error)]
pub enum SideEffectError {
    #[error("send failure: {0}")]
    Send(Box<dyn std::error::Error + Send + Sync>),
    #[error("target not found")]
    NotFound,
}
