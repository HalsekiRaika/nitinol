use crate::ident::ProcessName;
use crate::process::run;
use crate::process::{AnyProxy, Process, ProcessProxy, ProcessRegistry, Props};

pub struct ProcessSystem {
    registry: ProcessRegistry,
}

impl Default for ProcessSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSystem {
    pub fn new() -> Self {
        Self {
            registry: ProcessRegistry::new(),
        }
    }

    pub async fn spawn<P: Process>(&self, props: Props<P>) -> ProcessProxy<P> {
        let process = props.produce();
        run(process, None, self.registry.clone(), None).await
    }

    pub async fn spawn_named<P: Process>(
        &self,
        name: ProcessName,
        props: Props<P>,
    ) -> ProcessProxy<P> {
        let process = props.produce();
        run(process, Some(name), self.registry.clone(), None).await
    }

    pub async fn lookup(&self, pid: crate::ident::Pid) -> Option<AnyProxy> {
        self.registry.lookup(pid).await
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        self.registry.lookup_by_name(name).await
    }
}
