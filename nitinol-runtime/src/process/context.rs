use std::any::TypeId;

use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::{DeadLetterEnvelope, DeadLetterProxy};
use crate::process::message::Boxed;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::watch::{TerminatedReason, WatchRequest};

pub struct ProcessContext {
    pub(crate) pid: Pid,
    pub(crate) name: Option<ProcessName>,
    pub(crate) registry: ProcessRegistry,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
}

impl ProcessContext {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name.as_ref()
    }

    /// Start watching the process at `target_pid` for termination.
    ///
    /// If the target is alive, a `Watch` signal is sent to its lifecycle loop.
    /// If it is absent from the registry (already stopped), a `WatchRequest` is
    /// routed through `DeadLetterActor`, which responds with
    /// `Terminated { why: NotFound }`.
    pub async fn watch(&self, target_pid: Pid) {
        match self.registry.lookup(target_pid).await {
            Some(proxy) => {
                let result = proxy
                    .send_system_signal(SystemSignal::Watch {
                        watcher_pid: self.pid,
                    })
                    .await;
                if result.is_err() {
                    // Target stopped between lookup and signal delivery.
                    self.deliver_not_found(target_pid).await;
                }
            }
            None => {
                self.deliver_not_found(target_pid).await;
            }
        }
    }

    /// Stop watching the process at `target_pid`.
    ///
    /// No-op if the target is no longer in the registry.
    pub async fn unwatch(&self, target_pid: Pid) {
        if let Some(proxy) = self.registry.lookup(target_pid).await {
            let _ = proxy
                .send_system_signal(SystemSignal::Unwatch {
                    watcher_pid: self.pid,
                })
                .await;
        }
    }

    /// Route a `WatchRequest` through the dead-letter path to trigger
    /// `Terminated { why: NotFound }` back to this process.
    async fn deliver_not_found(&self, target_pid: Pid) {
        match &self.dead_letter {
            Some(dl) => {
                let envelope = DeadLetterEnvelope {
                    destination: target_pid,
                    message: Boxed::new(WatchRequest {
                        watched: target_pid,
                        watcher: self.pid,
                    }),
                    sender: Some(self.pid),
                    suppress_log: true,
                    message_type_id: TypeId::of::<WatchRequest>(),
                };
                dl.send(envelope).await;
            }
            None => {
                // No dead-letter actor available; send Terminated directly.
                let _ = self
                    .sys_tx
                    .send(SystemSignal::Terminated {
                        who: target_pid,
                        why: TerminatedReason::NotFound,
                    })
                    .await;
            }
        }
    }
}
