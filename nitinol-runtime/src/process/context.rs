use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::mpsc;

use crate::error::SendError;
use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::{Driver, PipeHandle, PipePanic, PipedTask};
use crate::process::pid_set::PidSet;
use crate::process::proxy::ProcessProxy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::spawn::SpawnEnv;
use crate::process::task::{TellTask, UserTask};
use crate::process::{Process, Props, Receive};

use super::wiring;

pub struct ProcessContext<P: Process> {
    pub(crate) pid: Pid,
    pub(crate) name: Option<ProcessName>,
    pub(crate) registry: ProcessRegistry,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
    pub(crate) self_proxy: ProcessProxy<P>,
    pub(crate) pipe_handle: Option<PipeHandle<P>>,
    pub(crate) parent: Option<Pid>,
    pub(crate) children: PidSet,
    pub(crate) default_idle_timeout: Option<Duration>,
    /// PIDs this process explicitly registered for DeathWatch via `ctx.watch`.
    ///
    /// Used by the lifecycle loop to distinguish hierarchy-only children (where
    /// `on_terminated` must NOT fire) from explicitly-watched children (where it
    /// MUST fire).  Maintained as interior-mutable state so `watch`/`unwatch` can
    /// keep `&self` signatures required by ARCH-REVIEW-004.
    pub(crate) explicit_watches: Mutex<HashSet<Pid>>,
}

impl<P: Process> ProcessContext<P> {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name.as_ref()
    }

    /// Pid of the process that spawned this one via `ctx.spawn_child*`.
    ///
    /// Returns `None` for top-level processes (those spawned via
    /// `ProcessSystem::spawn*`).
    pub fn parent(&self) -> Option<&Pid> {
        self.parent.as_ref()
    }

    /// The set of Pids spawned by this process via `ctx.spawn_child*`.
    ///
    /// Read-only — the runtime maintains the membership during the process's
    /// lifecycle (entries are auto-removed when a child terminates).
    pub fn children(&self) -> &PidSet {
        &self.children
    }

    /// Immutable reference to this process's own proxy.
    ///
    /// Clone if you need an owned handle (e.g. for Pipe-to-Self where a handler
    /// captures the proxy to schedule a follow-up `tell` to itself).
    pub fn self_proxy(&self) -> &ProcessProxy<P> {
        &self.self_proxy
    }

    /// Spawn `props` as a child of this process.
    ///
    /// The new process inherits the parent's [`ProcessRegistry`] (flat
    /// registry — no path information attached to the Pid) and the system
    /// default idle timeout used by [`crate::ProcessSystem::spawn`]. Its
    /// `ctx.parent()` returns this process's Pid. When this process stops,
    /// the runtime cascade-stops every child (reverse insertion order) and
    /// waits for their `Terminated` before exiting.
    pub async fn spawn_child<C: Process>(&mut self, props: Props<C>) -> ProcessProxy<C> {
        let env = SpawnEnv::child(
            self.registry.clone(),
            self.dead_letter.clone(),
            self.default_idle_timeout,
            self.pid,
        );
        let proxy = env.spawn(None, props).await;
        self.register_child(&proxy).await;
        proxy
    }

    /// Spawn `props` as a child of this process, driven by `driver`.
    ///
    /// Same parent/child semantics as [`Self::spawn_child`], but the caller
    /// supplies the driver tree — used for tick / poll sources where the
    /// default `MessageDriver + PipeDriver` composition is not appropriate
    /// (e.g. `IntervalDriver` backing a `DurableStream` poller).
    pub async fn spawn_child_with_driver<C, D>(
        &mut self,
        props: Props<C>,
        driver: D,
    ) -> ProcessProxy<C>
    where
        C: Process,
        D: Driver<C>,
    {
        let env = SpawnEnv::child(
            self.registry.clone(),
            self.dead_letter.clone(),
            self.default_idle_timeout,
            self.pid,
        );
        let proxy = env.spawn_with_driver(None, props, driver).await;
        self.register_child(&proxy).await;
        proxy
    }

    /// Record `proxy` as a child of this process.
    ///
    /// Adds the child Pid to `self.children` and registers this process as an
    /// implicit watcher so the child sends `Terminated` on exit (needed for
    /// `stop_children_and_wait` and `ctx.children` bookkeeping). NOT recorded
    /// in `explicit_watches`, so `on_terminated` is not triggered unless the
    /// user also calls `ctx.watch`.
    ///
    /// Uses `wiring::watch` rather than a direct signal send so that if the
    /// child has already terminated before the Watch is processed, a
    /// `Terminated { why: NotFound }` is delivered to this process's `sys_tx`.
    /// That guarantees `stop_children_and_wait` can drain the child even when
    /// it exits before the Watch registration completes.
    async fn register_child<C: Process>(&mut self, proxy: &ProcessProxy<C>) {
        let child_pid = proxy.pid();
        self.children.add(child_pid);
        wiring::watch(
            self.pid,
            child_pid,
            &self.registry,
            &self.sys_tx,
            self.dead_letter.as_ref(),
        )
        .await;
    }

    /// Start watching the process at `target_pid` for termination.
    ///
    /// If the target is alive, a `Watch` signal is sent to its lifecycle loop.
    /// If it is absent from the registry (already stopped), a `WatchRequest` is
    /// routed through `DeadLetterProcess`, which responds with
    /// `Terminated { why: NotFound }`.
    ///
    /// Calling `watch` causes `on_terminated` to be invoked when the target
    /// terminates, regardless of whether the target is also a child process.
    pub async fn watch(&self, target_pid: Pid) {
        self.explicit_watches.lock().unwrap().insert(target_pid);
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
        self.explicit_watches.lock().unwrap().remove(&target_pid);
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
