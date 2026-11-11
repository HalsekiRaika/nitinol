//! Errors returned by the runtime's operations.
//!
//! - [`SendError`] is returned when
//!   [`tell`](crate::process::ProcessProxy::tell) or
//!   [`stop`](crate::process::ProcessProxy::stop) finds the process already
//!   stopped.
//! - [`AskError`] is returned when [`ask`](crate::process::ProcessProxy::ask)
//!   cannot deliver the message or gets no reply, or when the handler returns
//!   an error.
//! - [`SpawnError`] is returned when a [`StreamProps`](crate::StreamProps) is
//!   spawned for a topic that already has a stream.
//! - [`StashError`] is returned when
//!   [`ProcessContext::stash`](crate::process::ProcessContext::stash) finds the
//!   stash full.
//! - [`ConfigError`] is returned when a configuration value such as
//!   [`MailboxCapacity::bounded`](crate::MailboxCapacity::bounded) or
//!   [`SupervisionStrategy::restart`](crate::SupervisionStrategy::restart) is
//!   given an invalid argument.
//!
//! A configuration value that is out of range is rejected when it is built:
//!
//! ```
//! use std::time::Duration;
//!
//! use nitinol_runtime::error::ConfigError;
//! use nitinol_runtime::{MailboxCapacity, SupervisionStrategy};
//!
//! assert!(matches!(
//!     MailboxCapacity::bounded(0),
//!     Err(ConfigError::ZeroCapacity)
//! ));
//! assert!(matches!(
//!     SupervisionStrategy::restart(3, Duration::ZERO),
//!     Err(ConfigError::InvalidRestartWithin)
//! ));
//!
//! assert!(MailboxCapacity::bounded(16).is_ok());
//! assert!(SupervisionStrategy::restart(3, Duration::from_secs(10)).is_ok());
//! ```

use crate::ident::Pid;

#[derive(Debug, thiserror::Error)]
#[error("process already stopped")]
pub struct SendError;

#[derive(Debug, thiserror::Error)]
pub enum AskError<E: std::error::Error> {
    #[error("dead letter: no process at pid {destination}")]
    DeadLetter { destination: Pid },
    #[error("process dropped reply")]
    ReplyDropped,
    #[error("ask timed out: no reply from pid {destination}")]
    Timeout { destination: Pid },
    #[error(transparent)]
    Handler(E),
}

#[derive(Debug, thiserror::Error)]
#[error("stream topic '{topic}' already registered")]
pub struct SpawnError {
    pub topic: String,
}

#[derive(Debug, thiserror::Error)]
#[error("handler returned an error")]
pub struct HandlerError;

/// Errors surfaced by [`ProcessContext::stash`](crate::process::ProcessContext::stash).
#[derive(Debug, thiserror::Error)]
pub enum StashError {
    /// The stash buffer is at its configured capacity; the message was not
    /// stashed. For an `ask`-originated message the reply channel is dropped,
    /// so the caller observes [`AskError::ReplyDropped`].
    #[error("stash buffer is full")]
    Full,
}

/// Errors surfaced by smart constructors on configuration values like
/// `MailboxCapacity::bounded(0)` or `SupervisionStrategy::restart(_, ZERO)`.
///
/// Sharing a single error type across capacity and supervision smart
/// constructors keeps `?` composition uniform in `Props` builder chains.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `*Capacity::bounded(0)` rejected: bounded capacity must be > 0 so a
    /// `NonZeroUsize` payload can be produced safely.
    #[error("bounded capacity must be > 0")]
    ZeroCapacity,
    /// `SupervisionStrategy::restart(_, Duration::ZERO)` rejected: the
    /// rate-limit window must be strictly positive.
    #[error("Restart `within` must be > 0")]
    InvalidRestartWithin,
}
