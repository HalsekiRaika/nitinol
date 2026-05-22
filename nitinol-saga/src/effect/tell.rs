use futures_core::future::BoxFuture;
use nitinol_eventsource::{Aggregate, AggregateProxy, Decider};

use crate::effect::core::SagaSideEffect;
use crate::error::SagaSideEffectError;

/// A side effect that sends a typed command to a target aggregate process.
///
/// The `A: Decider<C>` constraint is enforced at construction time
/// (via [`crate::SagaEffect::tell`]), so a type mismatch becomes a compile
/// error rather than a runtime failure.
pub(crate) struct TypedSagaTell<A: Aggregate, C> {
    pub(crate) target: AggregateProxy<A>,
    pub(crate) cmd: C,
}

impl<A, C> SagaSideEffect for TypedSagaTell<A, C>
where
    A: Aggregate + Decider<C>,
    C: Send + Sync + 'static,
{
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SagaSideEffectError>> {
        Box::pin(async move {
            self.target
                .tell(self.cmd)
                .await
                .map_err(|e| SagaSideEffectError::Send(Box::new(e)))
        })
    }
}
