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
