//! Error types for `nitinol-saga`.

/// Error returned from `Receive<UpstreamMessage>::recv` when the append that
/// settles an upstream-delivered record fails.
///
/// Returning this error value from the handler signals the manager's source
/// partition (via `ask()`) to read no further than the record before it,
/// ensuring the upstream message is never treated as processed even when the
/// `SagaProcess` subsequently stops: the same record is handed again on the
/// partition's next turn, and its offset never moves past it.
#[derive(Debug, thiserror::Error)]
#[error("the append settling this record failed; it must not be treated as processed")]
pub(crate) struct SagaUpstreamHandlerError;
