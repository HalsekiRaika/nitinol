use std::time::Duration;

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

    pub(crate) fn produce(&self) -> P {
        (self.producer)()
    }
}

#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    Stop,
    Restart {
        max_retries: u32,
        within: Duration,
    },
}
