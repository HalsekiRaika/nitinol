use std::time::Duration;

use crate::process::supervision::SupervisionConfig;
use crate::process::Process;

/// Controls the idle timeout behavior for a single process.
#[derive(Debug, Clone, Copy, Default)]
pub enum IdleTimeout {
    /// Inherit the system-level default idle timeout (no timeout if no system default is set).
    #[default]
    Inherit,
    /// Never time out regardless of the system default.
    Persistent,
    /// Time out after this duration of idle (no messages received).
    After(Duration),
}

pub struct Props<P: Process> {
    producer: Box<dyn Fn() -> P + Send + Sync>,
    supervision_strategy: SupervisionStrategy,
    idle_timeout: IdleTimeout,
}

impl<P: Process> Props<P> {
    pub fn new(producer: impl Fn() -> P + Send + Sync + 'static) -> Self {
        Self {
            producer: Box::new(producer),
            supervision_strategy: SupervisionStrategy::Stop,
            idle_timeout: IdleTimeout::default(),
        }
    }

    pub fn with_supervision_strategy(&mut self, strategy: SupervisionStrategy) -> &mut Self {
        self.supervision_strategy = strategy;
        self
    }

    pub fn with_idle_timeout(&mut self, timeout: IdleTimeout) -> &mut Self {
        self.idle_timeout = timeout;
        self
    }

    /// Produce the initial process instance, the `IdleTimeout` policy, and a
    /// `SupervisionConfig` for lifecycle management.
    pub(crate) fn into_parts(self) -> (P, IdleTimeout, SupervisionConfig<P>) {
        if let SupervisionStrategy::Restart { within, .. } = &self.supervision_strategy {
            assert!(
                !within.is_zero(),
                "SupervisionStrategy::Restart requires `within` > 0"
            );
        }
        let initial = (self.producer)();
        let config = SupervisionConfig {
            producer: self.producer,
            strategy: self.supervision_strategy,
        };
        (initial, self.idle_timeout, config)
    }
}

#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    Stop,
    Restart {
        max_retries: u32,
        within: Duration,
    },
    /// Ignore the handler error and continue processing later messages.
    Resume,
}

/// Resolve an `IdleTimeout` policy against the system-level default to a
/// concrete `Option<Duration>`.
///
/// Defined here (and re-used by `ProcessSystem::spawn*` and
/// `ProcessContext::spawn_child*`) so child processes inherit the system
/// default consistently with top-level spawns.
pub(crate) fn resolve_idle_timeout(
    idle_timeout: IdleTimeout,
    default_idle_timeout: Option<Duration>,
) -> Option<Duration> {
    match idle_timeout {
        IdleTimeout::After(dur) => Some(dur),
        IdleTimeout::Persistent => None,
        IdleTimeout::Inherit => default_idle_timeout,
    }
}
