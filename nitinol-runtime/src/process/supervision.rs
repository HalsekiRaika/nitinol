use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::process::props::SupervisionStrategy;
use crate::process::Process;

pub(crate) struct SupervisionConfig<P: Process> {
    pub(crate) producer: Box<dyn Fn() -> P + Send + Sync>,
    pub(crate) strategy: SupervisionStrategy,
}

/// Sliding-window restart counter.
///
/// Tracks restart timestamps in a `VecDeque`. On each call to
/// `should_restart`, timestamps outside the window are pruned first.
/// A restart is allowed when the in-window count is still below `max_retries`.
pub(crate) struct RestartTracker {
    timestamps: VecDeque<Instant>,
}

impl RestartTracker {
    pub(crate) fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
        }
    }

    /// Returns `true` and records the restart if the rate limit is not exceeded.
    /// Returns `false` when the limit has been reached (caller should stop).
    pub(crate) fn should_restart(&mut self, max_retries: u32, within: Duration) -> bool {
        let now = Instant::now();
        let cutoff = now - within;
        self.timestamps.retain(|&t| t > cutoff);
        self.timestamps.push_back(now);
        (self.timestamps.len() as u32) <= max_retries
    }
}
