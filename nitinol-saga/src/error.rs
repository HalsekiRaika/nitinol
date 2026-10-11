//! Error types for `nitinol-saga`.

/// Error returned from `Receive<UpstreamMessage>::recv` when the append that
/// settles an upstream-delivered record fails.
///
/// Returning this error value from the handler signals the delivery layer
/// (`DirectPollerProcess` via `ask()`) to NOT advance the upstream cursor,
/// ensuring the upstream message is never treated as processed even when the
/// `SagaProcess` subsequently stops, keeping delivery state consistent.
#[derive(Debug, thiserror::Error)]
#[error("the append settling this record failed; it must not be treated as processed")]
pub(crate) struct SagaUpstreamHandlerError;
