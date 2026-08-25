use std::time::Duration;

use crate::error::ConfigError;

/// Payload of [`SupervisionStrategy::Restart`]. Fields are private to prevent
/// external code from bypassing [`SupervisionStrategy::restart`] validation
/// (which rejects `Duration::ZERO`).
#[derive(Debug, Clone)]
pub struct RestartConfig {
    max_retries: u32,
    within: Duration,
}

impl RestartConfig {
    pub(crate) fn new(max_retries: u32, within: Duration) -> Self {
        Self {
            max_retries,
            within,
        }
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    pub fn within(&self) -> Duration {
        self.within
    }
}

/// Recovery policy applied when a `Receive`/`Driver::apply` handler returns
/// `Err`.
///
/// Constructed via the smart constructors [`Self::stop`], [`Self::resume`],
/// and [`Self::restart`]. The `Restart` variant carries a [`RestartConfig`]
/// with private fields so that external code cannot bypass the `restart()`
/// validation (which rejects `within = Duration::ZERO`).
#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    Stop,
    Restart(RestartConfig),
    /// Ignore the handler error and continue processing later messages.
    Resume,
}

impl SupervisionStrategy {
    /// Stop on handler error (run `on_stop`, no restart).
    pub fn stop() -> Self {
        Self::Stop
    }

    /// Swallow the handler error and keep processing subsequent messages.
    pub fn resume() -> Self {
        Self::Resume
    }

    /// Restart up to `max_retries` times within the rolling window `within`.
    ///
    /// Rejects `Duration::ZERO` for `within` eagerly.
    pub fn restart(max_retries: u32, within: Duration) -> Result<Self, ConfigError> {
        if within.is_zero() {
            return Err(ConfigError::InvalidRestartWithin);
        }
        Ok(Self::Restart(RestartConfig::new(max_retries, within)))
    }
}
