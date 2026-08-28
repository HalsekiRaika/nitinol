use nitinol_persistence::error::AppendError;

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

/// Error produced while writing the facts of an accepted decision to the stream.
///
/// A decision that reached this point was accepted, so nothing here is a verdict
/// on the command: these are failures of the machinery that was supposed to
/// record it (L-6).
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
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
    /// A creation collided with an aggregate that already exists (L-7).
    ///
    /// No decision was reached — the facts never landed — so there is no output
    /// to hand back, and the interpreter does not invent one.  Whether a
    /// redelivered creation is a success, a duplicate or a conflict is the
    /// caller's judgement to make, not this layer's.
    #[error("the aggregate has already been created")]
    AlreadyCreated,
    #[error("persisting the accepted decision failed: {0}")]
    Persist(PersistError),
    #[error("process send error: {0}")]
    Send(nitinol_runtime::error::SendError),
}

impl<R: std::error::Error + Send + Sync + 'static> AskError<R> {
    /// Whether re-issuing the command is worth attempting.
    pub fn retryability(&self) -> Retryability {
        match self {
            // The decider looked at the command and refused it.
            AskError::Rejection(_) => Retryability::Permanent,
            // An aggregate that exists will still exist on the next attempt, so
            // the same creation collides with the same first write again.
            AskError::AlreadyCreated => Retryability::Permanent,
            // The command never reached an activation, or reached one that was
            // stopping — including one stopped by a conflict of its own.
            AskError::Send(_) => Retryability::Transient,
            AskError::Persist(e) => e.retryability(),
        }
    }
}

impl PersistError {
    /// Whether the failure was about the machinery the append ran through,
    /// rather than about the facts it carried.
    fn retryability(&self) -> Retryability {
        match self {
            // Losing the sequence means another writer took it; a re-resolved
            // activation replays past it and can carry the command.
            Self::Append(AppendError::SequenceConflict(_)) => Retryability::Transient,
            // The store failed to answer.  Nothing was committed, and the fault
            // is the store's rather than the command's.
            Self::Append(AppendError::Backend(_)) => Retryability::Transient,
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
/// Mapped variant-for-variant to `AskError<R>` by `AggregateProxy`, which adds
/// only the failures it can observe itself.
#[derive(Debug, thiserror::Error)]
pub enum AskHandlerError<R: std::error::Error + Send + Sync + 'static> {
    #[error("rejection: {0}")]
    Rejection(R),
    #[error("the aggregate has already been created")]
    AlreadyCreated,
    #[error("persist: {0}")]
    Persist(PersistError),
}

/// Internal error produced by `AggregateProcess::recv` for query messages.
///
/// Mapped to `ExecError<E>` by `AggregateProxy`.
#[derive(Debug, thiserror::Error)]
pub enum ExecHandlerError<E: std::error::Error + Send + Sync + 'static> {
    #[error("domain error: {0}")]
    Domain(E),
}
