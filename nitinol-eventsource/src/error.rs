use nitinol_persistence::error::AppendError;

use crate::SideEffectError;

/// Error produced when decoding a [`crate::SystemEvent`] payload fails.
///
/// The underlying serializer (prost) is wrapped behind `Box<dyn Error>` so the
/// framework's choice of wire format does not leak into the public surface.
#[doc(hidden)]
#[derive(Debug, thiserror::Error)]
#[error("system event decode failed: {0}")]
pub struct SystemEventDecodeError(Box<dyn std::error::Error + Send + Sync>);

impl SystemEventDecodeError {
    #[doc(hidden)]
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(source))
    }
}

/// Error produced when a [`crate::codec::ErasedCodec`] operation fails.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("encode error: {0}")]
    Encode(Box<dyn std::error::Error + Send + Sync>),
    #[error("decode error: {0}")]
    Decode(Box<dyn std::error::Error + Send + Sync>),
}

/// Error produced by the effect interpreter when executing an `Effect`.
#[derive(Debug, thiserror::Error)]
pub enum EffectExecutionError {
    #[error("side effect execution failed: {0}")]
    Side(#[from] SideEffectError),
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("event store append failed: {0}")]
    Append(nitinol_persistence::error::AppendError),
}

/// Whether re-issuing a failed dispatch can plausibly reach a different outcome.
///
/// A reference outlives the activation it was pointing at, so a caller has to be
/// able to tell "this command was refused" from "this command never reached an
/// aggregate that could judge it" — and to tell them apart from the type rather
/// than from an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    /// The failure describes the circumstances of the dispatch, not the command.
    ///
    /// Nothing was decided about the command itself, so re-issuing it — against
    /// whichever activation the reference resolves next — may well succeed.
    Transient,
    /// The failure is a verdict on this command against this state.
    ///
    /// Re-issuing it reproduces the verdict; only a different command, or a
    /// state that has since moved, changes the answer.
    Permanent,
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

impl<R: std::error::Error + Send + Sync + 'static> AskError<R> {
    /// Whether re-issuing the command is worth attempting.
    pub fn retryability(&self) -> Retryability {
        match self {
            // The decider looked at the command and refused it.
            AskError::Rejection(_) => Retryability::Permanent,
            // The command never reached an activation, or reached one that was
            // stopping — including one stopped by a conflict of its own.
            AskError::Send(_) => Retryability::Transient,
            AskError::Effect(e) => e.retryability(),
        }
    }
}

impl EffectExecutionError {
    /// Whether the failure was about the environment the effect ran in, rather
    /// than about the effect itself.
    fn retryability(&self) -> Retryability {
        match self {
            // Losing the sequence means another writer took it; a re-resolved
            // activation replays past it and can carry the command.
            Self::Append(AppendError::SequenceConflict(_)) => Retryability::Transient,
            // The store failed to answer.  Nothing was committed, and the fault
            // is the store's rather than the command's.
            Self::Append(AppendError::Backend(_)) => Retryability::Transient,
            // A side effect's own failure says nothing about the command that
            // scheduled it.
            Self::Side(_) => Retryability::Transient,
            // Encoding this event is deterministic: it fails again, identically.
            Self::Codec(_) => Retryability::Permanent,
        }
    }
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
