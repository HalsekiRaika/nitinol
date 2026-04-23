use std::time::Duration;

use crate::process::supervision::SupervisionConfig;
use crate::process::Process;

pub struct Props<P: Process> {
    producer: Box<dyn Fn() -> P + Send + Sync>,
    supervision_strategy: SupervisionStrategy,
}

impl<P: Process> Props<P> {
    pub fn new(producer: impl Fn() -> P + Send + Sync + 'static) -> Self {
        Self {
            producer: Box::new(producer),
            supervision_strategy: SupervisionStrategy::Stop,
        }
    }

    pub fn with_supervision_strategy(&mut self, strategy: SupervisionStrategy) -> &mut Self {
        self.supervision_strategy = strategy;
        self
    }

    /// Produce the initial process instance and a `SupervisionConfig` for lifecycle management.
    pub(crate) fn into_parts(self) -> (P, SupervisionConfig<P>) {
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
        (initial, config)
    }
}

#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    Stop,
    Restart { max_retries: u32, within: Duration },
    /// Ignore the handler error and continue processing subsequent messages.
    Resume,
}
