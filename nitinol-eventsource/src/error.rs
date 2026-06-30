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
