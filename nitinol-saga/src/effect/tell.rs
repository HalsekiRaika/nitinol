use std::marker::PhantomData;

use futures_core::future::BoxFuture;
use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::SagaSideEffect;
use crate::error::SagaSideEffectError;

/// A side effect that sends a typed command to a target aggregate process.
///
/// The `A: Decider<C>` constraint is enforced at construction time
/// (via [`crate::SagaEffect::tell`]), so a type mismatch becomes a compile
/// error rather than a runtime failure.
pub(crate) struct TypedSagaTell<A, C, T>
where
    A: Aggregate,
    T: AggregateTellTarget<A>,
{
    pub(crate) target: T,
    pub(crate) cmd: C,
    pub(crate) _phantom: PhantomData<fn() -> A>,
}

impl<A, C, T> SagaSideEffect for TypedSagaTell<A, C, T>
where
    A: Aggregate + Decider<C>,
    C: Send + Sync + 'static,
    T: AggregateTellTarget<A>,
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
