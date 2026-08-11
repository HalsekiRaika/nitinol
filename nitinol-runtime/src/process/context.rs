use std::any::Any;
use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::mpsc;

use crate::error::{SendError, StashError};
use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::{PipeHandle, PipePanic, PipedTask, StashHandle};
use crate::process::pid_set::PidSet;
use crate::process::props::{MailboxCapacity, PipeCapacity, StashCapacity};
use crate::process::proxy::ProcessProxy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::spawn::SpawnEnv;
use crate::process::spawnable::SpawnDispatch;
use crate::process::task::{AskTask, ReplySender, TellTask, UserTask};
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
    pub(crate) stash_handle: Option<StashHandle<P>>,
    pub(crate) pending_reply: Option<Box<dyn Any + Send + Sync>>,
    pub(crate) parent: Option<Pid>,
    pub(crate) children: PidSet,
    pub(crate) default_idle_timeout: Option<Duration>,
    pub(crate) default_mailbox_capacity: MailboxCapacity,
    pub(crate) default_stash_capacity: StashCapacity,
    pub(crate) default_pipe_capacity: PipeCapacity,
    /// PIDs this process explicitly registered for DeathWatch via `ctx.watch`.
    ///
    /// Used by the lifecycle loop to distinguish hierarchy-only children (where
    /// `on_terminated` must NOT fire) from explicitly-watched children (where it
    /// MUST fire).  Maintained as interior-mutable state so `watch`/`unwatch` can
    /// keep the `&self` signatures the watch/unwatch contract requires.
    pub(crate) explicit_watches: Mutex<HashSet<Pid>>,
}

impl<P: Process> ProcessContext<P> {
    /// Record `pid` as explicitly watched, so its `Terminated` reaches
    /// `on_terminated` even when it is also a hierarchy child.
    pub(crate) fn track_explicit_watch(&self, pid: Pid) {
        self.lock_explicit_watches().insert(pid);
    }

    /// Drop `pid` from the explicit-watch set, returning whether it was there.
    ///
    /// The lifecycle loop uses the return value to decide whether a terminated
    /// child was explicitly watched or hierarchy-only.
    pub(crate) fn untrack_explicit_watch(&self, pid: &Pid) -> bool {
        self.lock_explicit_watches().remove(pid)
    }

    fn lock_explicit_watches(&self) -> MutexGuard<'_, HashSet<Pid>> {
        self.explicit_watches
            .lock()
            .expect("explicit watch set lock was poisoned by a panicking holder")
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name.as_ref()
    }

    /// Pid of the process that spawned this one via `ctx.spawn_child`.
    ///
    /// Returns `None` for top-level processes (those spawned via
    /// `ProcessSystem::spawn`).
    pub fn parent(&self) -> Option<&Pid> {
        self.parent.as_ref()
    }

    /// The set of Pids spawned by this process via `ctx.spawn_child`.
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

    /// Spawn `spawnable` as a child of this process.
    ///
    /// The new process inherits the parent's [`ProcessRegistry`] (flat
    /// registry — no path information attached to the Pid) and the system
    /// default idle timeout used by [`crate::ProcessSystem::spawn`]. Its
    /// `ctx.parent()` returns this process's Pid. When this process stops,
    /// the runtime cascade-stops every child (reverse insertion order) and
    /// waits for their `Terminated` before exiting.
    ///
    /// Accepts `Props<C>` (returns `ProcessProxy<C>`) or `StreamProps<T>`
    /// (returns `Result<ProcessProxy<Stream<T>>, SpawnError>`).
    #[allow(private_bounds)]
    pub async fn spawn_child<S: SpawnDispatch>(&mut self, spawnable: S) -> S::Output {
        let env = SpawnEnv::child(
            self.registry.clone(),
            self.dead_letter.clone(),
            self.default_idle_timeout,
            self.default_mailbox_capacity,
            self.default_stash_capacity,
            self.default_pipe_capacity,
            self.pid,
        );
        let registry = env.registry().clone();
        let output = spawnable.spawn_with(&env).await;
        let child_pid = match S::child_pid(&output) {
            Some(pid) => pid,
            None => return output,
        };
        self.register_child_by_pid(child_pid, &registry).await;
        output
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
    async fn register_child_by_pid(&mut self, child_pid: Pid, registry: &ProcessRegistry) {
        self.children.add(child_pid);
        wiring::watch(
            self.pid,
            child_pid,
            registry,
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
        self.track_explicit_watch(target_pid);
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
        self.untrack_explicit_watch(&target_pid);
        wiring::unwatch(self.pid, target_pid, &self.registry).await;
    }

    /// Save the in-flight message into the stash for later re-delivery.
    ///
    /// Stashed messages are replayed — ahead of newly arrived messages, in
    /// stash order — when [`Self::unstash_all`] or [`Self::unstash`] is called.
    /// An `ask`-originated message keeps its reply channel while stashed, so
    /// the caller stays pending until the message is replayed and handled.
    ///
    /// # Errors
    ///
    /// Returns [`StashError::Full`] when the stash is already at its configured
    /// [`StashCapacity`](crate::StashCapacity). The message is not stashed: a
    /// `tell` is discarded, and an `ask` has its reply channel dropped so the
    /// caller observes [`AskError::ReplyDropped`](crate::error::AskError::ReplyDropped).
    pub async fn stash<M>(&mut self, msg: M) -> Result<(), StashError>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
    {
        let task: UserTask<P> = match self.take_pending_reply::<M>() {
            Some(reply_tx) => Box::new(AskTask::new(msg, reply_tx)),
            None => Box::new(TellTask::new(msg)),
        };
        match self.stash_handle().try_stash(task) {
            Ok(()) => Ok(()),
            Err(_overflowed) => Err(StashError::Full),
        }
    }

    /// Re-deliver every stashed message ahead of new arrivals, in stash order.
    pub async fn unstash_all(&mut self) {
        self.stash_handle().unstash_all();
    }

    /// Re-deliver the first `n` stashed messages ahead of new arrivals, in
    /// stash order; the remainder stay stashed. Fewer than `n` are replayed
    /// when the stash holds fewer messages.
    pub async fn unstash(&mut self, n: usize) {
        self.stash_handle().unstash(n);
    }

    fn stash_handle(&self) -> &StashHandle<P> {
        self.stash_handle.as_ref().expect(
            "ProcessContext stash API called on a process whose driver tree \
             lacks StashDriver — lifecycle::run is supposed to always compose \
             the Core trio (Message + Pipe + Stash)",
        )
    }

    pub(crate) fn set_pending_reply(&mut self, reply: Box<dyn Any + Send + Sync>) {
        self.pending_reply = Some(reply);
    }

    pub(crate) fn take_pending_reply<M>(&mut self) -> Option<ReplySender<P, M>>
    where
        P: Receive<M>,
        M: 'static,
    {
        let parked = self.pending_reply.take()?;
        match parked.downcast::<ReplySender<P, M>>() {
            Ok(reply_tx) => Some(*reply_tx),
            Err(parked) => {
                self.pending_reply = Some(parked);
                None
            }
        }
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
    /// # Bounded backpressure
    ///
    /// Enqueue is `try_send` over the `PipeDriver`'s bounded channel: when
    /// the buffer is full or the channel is closed, the future is dropped
    /// silently and the actor stays alive.
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
            // Unreachable in normal operation: `lifecycle::run` always
            // composes a `PipeDriver` into the Core driver trio, so every
            // spawned process has a pipe handle.
            None => unreachable!(
                "ProcessContext::pipe_to_self called on a process whose driver tree \
                 lacks PipeDriver — lifecycle::run is supposed to always compose \
                 the Core trio (Message + Pipe + Stash)"
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

        // try_send: on full (bounded buffer exhausted) or on closed
        // (lifecycle terminated), drop silently. Panicking here would crash
        // the still-running shutdown path or surprise the user on a
        // bounded-buffer overload; silent drop matches the plan's
        // pipe-to-self bounded migration.
        let _ = handle.tx.try_send(Box::pin(task_fut));
    }
}
