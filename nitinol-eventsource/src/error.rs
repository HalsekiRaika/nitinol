use crate::SideEffectError;
use crate::process::EncodeError;

/// Identifies why a persistence actor was unreachable.
///
/// Preserves the distinction between a dead-letter (actor never existed or
/// already stopped) and a reply-dropped (actor started handling the message
/// but the reply channel was dropped before the response arrived).
#[derive(Debug, thiserror::Error)]
pub enum PersistorUnreachableKind {
    #[error("no process at pid {destination}")]
    DeadLetter {
        destination: nitinol_runtime::ident::Pid,
    },
    #[error("process dropped the reply channel")]
    ReplyDropped,
}

/// Error produced by the effect interpreter when executing an `Effect`.
#[derive(Debug, thiserror::Error)]
pub enum EffectExecutionError {
    #[error("side effect execution failed: {0}")]
    Side(#[from] SideEffectError),
    #[error("event encode failed: {0}")]
    Encode(#[from] EncodeError),
    #[error("event store append failed: {0}")]
    Append(nitinol_persistence::error::AppendError),
    #[error("persistence actor unreachable: {0}")]
    PersistorUnreachable(PersistorUnreachableKind),
}

/// Error returned by `AggregateProxy::ask`.
#[derive(Debug, thiserror::Error)]
pub enum AskError<R: std::error::Error + Send + Sync + 'static> {
    #[error("command rejected: {0}")]
    Rejection(R),
    #[error("effect execution failed: {0}")]
    Effect(EffectExecutionError),
    #[error("process send error: {0}")]
    Send(nitinol_runtime::error::SendError),
}

/// Error returned by `AggregateProxy::tell`.
#[derive(Debug, thiserror::Error)]
pub enum TellError {
    #[error("process send error: {0}")]
    Send(#[from] nitinol_runtime::error::SendError),
}

/// Error returned by `AggregateProxy::exec`.
#[derive(Debug, thiserror::Error)]
pub enum ExecError<E: std::error::Error + Send + Sync + 'static> {
    #[error("domain error: {0}")]
    Domain(E),
    #[error("process send error: {0}")]
    Send(nitinol_runtime::error::SendError),
}

/// Internal error produced by `AggregateProcess::recv` for command messages.
///
/// Mapped to `AskError<R>` by `AggregateProxy`.
#[derive(Debug, thiserror::Error)]
pub enum AskHandlerError<R: std::error::Error + Send + Sync + 'static> {
    #[error("rejection: {0}")]
    Rejection(R),
    #[error("effect: {0}")]
    Effect(#[from] EffectExecutionError),
}

/// Internal error produced by `AggregateProcess::recv` for query messages.
///
/// Mapped to `ExecError<E>` by `AggregateProxy`.
#[derive(Debug, thiserror::Error)]
pub enum ExecHandlerError<E: std::error::Error + Send + Sync + 'static> {
    #[error("domain error: {0}")]
    Domain(E),
}
