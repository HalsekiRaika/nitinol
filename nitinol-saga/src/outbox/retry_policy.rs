use std::time::Duration;

/// Retry policy used by the outbox tell executor.
///
/// Default contract (locked by plan §4):
/// - `max_attempts`: 3 (1 initial attempt + 2 retries)
/// - `initial_backoff`: 100 ms
/// - `multiplier`: 2.0 (exponential backoff: 100 ms, 200 ms, ...)
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    pub(crate) max_attempts: usize,
    pub(crate) initial_backoff: Duration,
    pub(crate) multiplier: f64,
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
    /// Compute the backoff duration to wait *before* the attempt at
    /// 1-based index `next_attempt` (after the previous one failed).
    ///
    /// `next_attempt` is the upcoming attempt number (2, 3, ...).  The first
    /// attempt has no preceding wait, so the executor only calls this for
    /// `next_attempt >= 2`.
    pub(crate) fn backoff_before(&self, next_attempt: usize) -> Duration {
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
