use std::marker::PhantomData;

use futures_core::future::BoxFuture;
use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider};

use crate::effect::core::SagaSideEffect;
use crate::error::SagaSideEffectError;

/// A side effect that sends a typed command to a target aggregate process.
///
/// The `A: Decider<C>` constraint is enforced at construction time
/// (via [`crate::SagaEffect::tell`] / [`crate::TellIntent::new`]), so a type
/// mismatch becomes a compile error rather than a runtime failure.
///
/// `C: Clone` is required because the retry executor keeps the command in
/// memory across attempts and re-`tell`s with a cloned copy on every retry.
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
    C: Clone + Send + Sync + 'static,
    T: AggregateTellTarget<A>,
    <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
{
    fn execute_once(&self) -> BoxFuture<'_, Result<(), SagaSideEffectError>> {
        let cmd = self.cmd.clone();
        Box::pin(async move {
            self.target
                .tell(cmd)
                .await
                .map_err(|e| SagaSideEffectError::Send(Box::new(e)))
        })
    }
}
