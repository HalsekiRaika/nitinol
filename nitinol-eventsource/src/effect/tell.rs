use futures_core::future::BoxFuture;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};

use crate::effect::core::{SideEffect, SideEffectError};

/// A side effect that sends a typed message to a target process.
///
/// The `P: Process + Receive<M>` constraint is enforced at construction time
/// (via `Effect::tell`), so a type mismatch becomes a compile error rather than
/// a runtime failure.
pub(crate) struct TypedTell<P: Process, M> {
    pub(crate) target: ProcessProxy<P>,
    pub(crate) message: M,
}

impl<P, M> SideEffect for TypedTell<P, M>
where
    P: Process + Receive<M>,
    M: Send + Sync + 'static,
{
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.target
                .tell(self.message)
                .await
                .map_err(|e| SideEffectError::Send(Box::new(e)))
        })
    }
}
