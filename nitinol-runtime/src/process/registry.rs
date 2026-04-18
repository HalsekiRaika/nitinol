use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::ident::{Pid, ProcessName};
use crate::process::AnyProxy;

#[derive(Clone)]
pub(crate) struct ProcessRegistry(Arc<RegistryInner>);

struct RegistryInner {
    processes: RwLock<HashMap<Pid, AnyProxy>>,
    aliases: RwLock<HashMap<ProcessName, Pid>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self(Arc::new(RegistryInner {
            processes: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
        }))
    }

    pub async fn register(
        &self,
        pid: Pid,
        proxy: AnyProxy,
        name: Option<&ProcessName>,
    ) {
        self.0.processes.write().await.insert(pid, proxy);
        if let Some(name) = name {
            self.0.aliases.write().await.insert(name.clone(), pid);
        }
    }

    pub async fn unregister(&self, pid: Pid, name: Option<&ProcessName>) {
        self.0.processes.write().await.remove(&pid);
        if let Some(name) = name {
            self.0.aliases.write().await.remove(name);
        }
    }

    pub async fn lookup(&self, pid: Pid) -> Option<AnyProxy> {
        self.0.processes.read().await.get(&pid).cloned()
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        let pid = self.0.aliases.read().await.get(name).copied()?;
        self.lookup(pid).await
    }
}
