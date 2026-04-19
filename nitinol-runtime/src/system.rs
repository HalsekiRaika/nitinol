use crate::error::BoxError;
use crate::ident::ProcessName;
use crate::process::run;
use crate::process::{AnyProxy, Process, ProcessProxy, ProcessRegistry, Props, Stream};

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

    /// Spawn a `Stream<T>` process registered under `topic`.
    ///
    /// Returns an error if a process with the same topic name is already registered.
    pub async fn spawn_stream<T: 'static + Send + Sync>(
        &self,
        topic: ProcessName,
    ) -> Result<ProcessProxy<Stream<T>>, BoxError> {
        if self.registry.lookup_by_name(&topic).await.is_some() {
            return Err(format!("stream topic '{}' already registered", topic).into());
        }
        let process = Stream::new();
        let proxy = run(process, Some(topic), self.registry.clone(), None).await;
        Ok(proxy)
    }

    pub async fn lookup(&self, pid: crate::ident::Pid) -> Option<AnyProxy> {
        self.registry.lookup(pid).await
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        self.registry.lookup_by_name(name).await
    }
}
