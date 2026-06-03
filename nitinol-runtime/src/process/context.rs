use tokio::sync::mpsc;

use crate::error::SendError;
use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::proxy::ProcessProxy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::Process;

use super::wiring;

pub struct ProcessContext<P: Process> {
    pub(crate) pid: Pid,
    pub(crate) name: Option<ProcessName>,
    pub(crate) registry: ProcessRegistry,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
    pub(crate) self_proxy: ProcessProxy<P>,
}

impl<P: Process> ProcessContext<P> {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name.as_ref()
    }

    /// Immutable reference to this process's own proxy.
    ///
    /// Clone if you need an owned handle (e.g. for Pipe-to-Self where a handler
    /// captures the proxy to schedule a follow-up `tell` to itself).
    pub fn self_proxy(&self) -> &ProcessProxy<P> {
        &self.self_proxy
    }

    /// Start watching the process at `target_pid` for termination.
    ///
    /// If the target is alive, a `Watch` signal is sent to its lifecycle loop.
    /// If it is absent from the registry (already stopped), a `WatchRequest` is
    /// routed through `DeadLetterProcess`, which responds with
    /// `Terminated { why: NotFound }`.
    pub async fn watch(&self, target_pid: Pid) {
        wiring::watch(
            self.pid,
            target_pid,
            &self.registry,
            &self.sys_tx,
            self.dead_letter.as_ref(),
        )
        .await;
    }

    pub async fn stop_self(&self) -> Result<(), SendError> {
        wiring::stop_self(&self.sys_tx).await
    }

    /// Stop watching the process at `target_pid`.
    ///
    /// No-op if the target is no longer in the registry.
    pub async fn unwatch(&self, target_pid: Pid) {
        wiring::unwatch(self.pid, target_pid, &self.registry).await;
    }
}
