use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tokio::sync::mpsc;

use crate::error::SendError;
use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::{PipeHandle, PipePanic, PipedTask};
use crate::process::proxy::ProcessProxy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::task::{TellTask, UserTask};
use crate::process::{Process, Receive};

use super::wiring;

pub struct ProcessContext<P: Process> {
    pub(crate) pid: Pid,
    pub(crate) name: Option<ProcessName>,
    pub(crate) registry: ProcessRegistry,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
    pub(crate) self_proxy: ProcessProxy<P>,
    pub(crate) pipe_handle: Option<PipeHandle<P>>,
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

    /// Pipe the future `fut`'s result back into this actor as a typed message.
    ///
    /// The future is polled by the owning process's `PipeDriver` inside the
    /// same lifecycle loop task (single-threaded). When the future resolves
    /// — successfully or with a panic — `map` is invoked with
    /// `Ok(value)` / `Err(PipePanic)` to produce the follow-up message `M`,
    /// which is then delivered via `Receive<M>` just like any other tell.
    ///
    /// # Continuation, not reentrancy
    ///
    /// The current handler returns BEFORE the piped follow-up is processed;
    /// the mailbox stays single-threaded.
    ///
    /// # Panic capture
    ///
    /// A panic during the future's poll is caught and surfaced as
    /// `Err(PipePanic)` — the actor stays alive. CPU-bound or blocking
    /// futures are NOT supported and will stall the lifecycle loop.
    ///
    /// # No abort
    ///
    /// Pipe futures cannot be cancelled individually; they are dropped en
    /// masse when the lifecycle loop terminates.
    ///
    /// # Panics
    ///
    /// Panics if the actor was started without a `PipeDriver` composed into
    /// its driver tree (i.e. `spawn_with_driver` was used and the caller did
    /// not include `PipeDriver::new()` via `combine_drivers!`). Silent drop
    /// would mask programmer errors, so the misuse is reported immediately.
    pub fn pipe_to_self<F, M, U>(&self, fut: F, map: U)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
        M: 'static + Send + Sync,
        U: FnOnce(Result<F::Output, PipePanic>) -> M + Send + 'static,
        P: Receive<M>,
    {
        let handle = match &self.pipe_handle {
            Some(h) => h,
            None => panic!(
                "ProcessContext::pipe_to_self called but no PipeDriver is composed; \
                 include PipeDriver::new() in your driver tree via combine_drivers! \
                 (or use ProcessSystem::spawn / spawn_named, which compose it automatically)"
            ),
        };

        let task_fut = async move {
            let result = AssertUnwindSafe(fut)
                .catch_unwind()
                .await
                .map_err(PipePanic::new);
            let msg = map(result);
            let task: UserTask<P> = Box::new(TellTask::new(msg));
            PipedTask::new(task)
        };

        // Send failures here mean the receiving PipeDriver was already
        // dropped (lifecycle terminated). The future is dropped along with
        // the send error — there is nothing meaningful to do, and panicking
        // would crash the still-running shutdown path.
        let _ = handle.tx.send(Box::pin(task_fut));
    }
}
