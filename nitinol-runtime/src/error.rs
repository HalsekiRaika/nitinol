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
