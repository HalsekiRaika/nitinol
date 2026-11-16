use std::time::Duration;

/// How many attempts a bounded retry makes, and how long it waits before each
/// one after the first.
///
/// [`AggregateProxy::ask_with_retry`](crate::AggregateProxy::ask_with_retry)
/// does not write the same decision again: each attempt is a fresh `ask`, so an
/// activation that lost its stream is replaced by one that replays what the
/// winner wrote, and the command is decided again against that newer state.
/// Only a failure whose [`retryability`](crate::AskError::retryability) is
/// [`Transient`](crate::Retryability::Transient) is retried; a
/// [`Permanent`](crate::Retryability::Permanent) one would end the same way on
/// every attempt, so it is returned as it is.
///
/// The default makes 3 attempts, waiting 100 ms before the second and 200 ms
/// before the third.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_attempts: usize,
    initial_backoff: Duration,
    multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// A policy making at most `max_attempts` attempts, waiting
    /// `initial_backoff` before the second and growing that wait by
    /// `multiplier` before each one after.
    ///
    /// The first attempt is always made, so a `max_attempts` of 0 makes one
    /// attempt, as 1 does.
    pub fn new(max_attempts: usize, initial_backoff: Duration, multiplier: f64) -> Self {
        Self {
            max_attempts,
            initial_backoff,
            multiplier,
        }
    }

    /// The most attempts a retry makes, the first one included.
    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    /// How long to wait before the attempt numbered `next_attempt`, counting
    /// from 1. There is no wait before attempt 1; values below 2 give
    /// `initial_backoff`.
    pub fn backoff_before(&self, next_attempt: usize) -> Duration {
        // next_attempt = 2 → factor = multiplier^0 = 1.0
        // next_attempt = 3 → factor = multiplier^1
        let exponent = next_attempt.saturating_sub(2) as i32;
        let factor = self.multiplier.powi(exponent);
        let nanos = self.initial_backoff.as_nanos() as f64 * factor;
        // Clamp at u64::MAX nanoseconds to stay representable.
        if nanos >= u64::MAX as f64 {
            Duration::from_nanos(u64::MAX)
        } else {
            Duration::from_nanos(nanos as u64)
        }
    }
}
