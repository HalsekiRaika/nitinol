//! Error types for `nitinol-saga`.

/// Error produced when a saga side effect fails to execute.
///
/// The interpreter logs this error and continues — consistent with how
/// `Effect::Side` behaves in `nitinol-eventsource` ("MVP just logs").
#[derive(Debug, thiserror::Error)]
pub(crate) enum SagaSideEffectError {
    #[error("send failure: {0}")]
    Send(Box<dyn std::error::Error + Send + Sync>),
}

/// Error returned from `Receive<UpstreamMessage>::recv` when a dead-letter
/// append fails for an upstream-delivered message.
///
/// Returning this error value from the handler signals the delivery layer
/// (`DirectPollerProcess` via `ask()`) to NOT advance the upstream cursor,
/// ensuring the upstream message is never treated as processed even when the
/// `SagaProcess` subsequently stops, keeping delivery state consistent.
#[derive(Debug, thiserror::Error)]
#[error("dead-letter append failed; upstream message must not be treated as processed")]
pub(crate) struct SagaUpstreamHandlerError;
