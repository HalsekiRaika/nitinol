use crate::SideEffectError;

/// Error produced by the effect interpreter when executing an `Effect`.
#[derive(Debug, thiserror::Error)]
pub enum EffectExecutionError {
    #[error("side effect execution failed: {0}")]
    Side(#[from] SideEffectError),
}
