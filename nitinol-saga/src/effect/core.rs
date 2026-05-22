use futures_core::future::BoxFuture;

use crate::error::SagaSideEffectError;

/// A declarative description of effects produced by a [`crate::Saga::handle`]
/// call.
///
/// `SagaEffect<E>` is a Monoid: `None` is the identity element and `combine`
/// is the associative binary operation.  The `Sequence` variant accumulates
/// leaves in a flat list; `combine` introduces no nesting.
///
/// The effect namespace is intentionally separate from `nitinol_eventsource::Effect`:
/// at the call site the difference between an aggregate's `decide`-effect and a
/// saga's `handle`-effect is visible by type alone.
pub enum SagaEffect<E> {
    /// No-op — the identity element of the Monoid.
    None,
    /// Persist the given saga events to the saga's own event store, then
    /// apply each one to the in-memory saga state.
    Persist(Vec<E>),
    /// Execute a command against another aggregate (fire-and-forget).
    ///
    /// Constructed via [`SagaEffect::tell`]; the inner [`SagaTellEffect`] is
    /// opaque — its internal side effect cannot be constructed directly by
    /// user code.
    Tell(SagaTellEffect),
    /// An ordered collection of effects executed sequentially.
    Sequence(Vec<SagaEffect<E>>),
}

/// Opaque wrapper around a saga side effect produced by [`SagaEffect::tell`].
///
/// This type is public so that code outside the crate can match on the `Tell`
/// variant, but its inner field is private — `SagaEffect::tell` is the only
/// way to construct it.
pub struct SagaTellEffect(pub(crate) Box<dyn SagaSideEffect>);

/// A type-safe, object-safe side effect that can be executed asynchronously
/// from within the saga effect interpreter.
///
/// Constructed internally by [`SagaEffect::tell`]; not part of the public API.
pub(crate) trait SagaSideEffect: Send + Sync + 'static {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SagaSideEffectError>>;
}
