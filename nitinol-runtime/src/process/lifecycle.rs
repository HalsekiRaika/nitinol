use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::Either;
use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::{Combine, Driver, MessageDriver, PipeDriver};
use crate::process::pid_set::PidSet;
use crate::process::props::SupervisionStrategy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::supervision::{RestartTracker, SupervisionConfig};
use crate::process::task::UserTask;
use crate::process::watch::{Terminated, TerminatedReason};
use crate::process::{Process, ProcessContext, ProcessProxy};

/// Lifecycle-initialization parameters bundled as a single contract.
///
/// Groups the two fields that are constant across all spawns within one
/// call site (`parent` and `default_idle_timeout`) so they travel together
/// rather than as separate positional arguments.
pub(crate) struct LifecycleInit {
    pub(crate) parent: Option<Pid>,
    pub(crate) default_idle_timeout: Option<Duration>,
}

pub(crate) async fn run<P: Process>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    timeout: Option<Duration>,
    dead_letter: Option<DeadLetterProxy>,
    supervision: SupervisionConfig<P>,
    init: LifecycleInit,
) -> ProcessProxy<P> {
    let (user_tx, user_rx) = mpsc::channel::<UserTask<P>>(32);
    // `spawn` / `spawn_named` auto-compose the PipeDriver so that
    // `ctx.pipe_to_self` is wired without the user having to remember to
    // do it themselves. `spawn_with_driver` stays explicit (per plan): if
    // a caller wants pipe support there, they must include `PipeDriver` via
    // `combine_drivers!`.
    let driver = Combine::new(MessageDriver::new(user_rx), PipeDriver::<P>::new());
    run_with_driver(
        process,
        process_name,
        registry,
        user_tx,
        driver,
        timeout,
        dead_letter,
        supervision,
        init,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_driver<P: Process, D: Driver<P>>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    user_tx: mpsc::Sender<UserTask<P>>,
    driver: D,
    timeout: Option<Duration>,
    dead_letter: Option<DeadLetterProxy>,
    supervision: SupervisionConfig<P>,
    init: LifecycleInit,
) -> ProcessProxy<P> {
    let (sys_tx, sys_rx) = mpsc::channel::<SystemSignal>(32);

    let pid = Pid::next();

    let proxy = ProcessProxy {
        pid,
        user_tx,
        sys_tx: sys_tx.clone(),
        dead_letter: dead_letter.clone(),
        registry: registry.clone(),
    };

    let any_proxy = proxy.clone().into();
    registry
        .register(pid, any_proxy, process_name.as_ref())
        .await;

    #[cfg(tokio_unstable)]
    let task_name = match &process_name {
        Some(name) => format!("process-{}", name),
        None => format!("process-{}", pid),
    };

    let fut = lifecycle_loop(
        process,
        process_name,
        registry,
        pid,
        driver,
        sys_tx,
        sys_rx,
        timeout,
        dead_letter,
        supervision,
        proxy.clone(),
        init,
    );

    #[cfg(not(tokio_unstable))]
    tokio::spawn(fut);

    #[cfg(tokio_unstable)]
    {
        let _ = tokio::task::Builder::new()
            .name(&task_name)
            .spawn(fut)
            .expect("unexpected error occurred from tokio-runtime.");
    }

    proxy
}

#[allow(clippy::too_many_arguments)]
async fn lifecycle_loop<P: Process, D: Driver<P>>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    pid: Pid,
    driver: D,
    sys_tx: mpsc::Sender<SystemSignal>,
    sys_rx: mpsc::Receiver<SystemSignal>,
    timeout: Option<Duration>,
    dead_letter: Option<DeadLetterProxy>,
    supervision: SupervisionConfig<P>,
    self_proxy: ProcessProxy<P>,
    init: LifecycleInit,
) {
    let mut state = process;
    let mut driver = driver;
    let mut sys_rx = sys_rx;
    let mut watchers: HashSet<Pid> = HashSet::new();
    let mut restart_tracker = RestartTracker::new();

    // Extract the pipe handle (if any) from the driver tree once, at start.
    // `Combine` surfaces the first non-`None` handle in its subtree; drivers
    // without `PipeDriver` composed will yield `None` here, and any later
    // call to `ctx.pipe_to_self` will panic (programmer-error behavior).
    let pipe_handle = driver.pipe_handle();

    let mut ctx = ProcessContext {
        pid,
        name: process_name.clone(),
        registry: registry.clone(),
        sys_tx: sys_tx.clone(),
        dead_letter: dead_letter.clone(),
        self_proxy,
        pipe_handle,
        parent: init.parent,
        children: PidSet::new(),
        default_idle_timeout: init.default_idle_timeout,
        explicit_watches: Mutex::new(HashSet::new()),
    };

    // A driver that opts out (e.g., tick / poll sources) has no meaningful
    // "idle" notion, so the idle-timeout timer must stay disarmed even when
    // the caller configured `IdleTimeout::After(_)`.
    let timeout = if driver.supports_idle_timeout() {
        timeout
    } else {
        None
    };

    let timeout_fn = move || match timeout {
        Some(dur) => Either::Left(tokio::time::sleep(dur)),
        None => Either::Right(std::future::pending::<()>()),
    };

    tokio::pin! {
        let timeout = timeout_fn();
    }

    state.on_start(&mut ctx).await;

    // Set when `on_stop` is called at the start of a restart sequence so the
    // post-loop cleanup knows not to call it again for the same state.
    let mut on_stop_called_in_restart = false;

    let reason: TerminatedReason = loop {
        tokio::select! {
            biased;
            Some(sys_sig) = sys_rx.recv() => {
                match sys_sig {
                    SystemSignal::Stop => break TerminatedReason::Stopped,
                    SystemSignal::Poison => break TerminatedReason::Poisoned,
                    SystemSignal::Watch { watcher_pid } => {
                        watchers.insert(watcher_pid);
                    }
                    SystemSignal::Unwatch { watcher_pid } => {
                        watchers.remove(&watcher_pid);
                    }
                    SystemSignal::Terminated { who, why } => {
                        if ctx.children.contains(&who) {
                            ctx.children.remove(&who);
                            let was_explicit =
                                ctx.explicit_watches.lock().unwrap().remove(&who);
                            if was_explicit {
                                state.on_terminated(Terminated { who, why }, &mut ctx).await;
                            }
                        } else {
                            ctx.explicit_watches.lock().unwrap().remove(&who);
                            state.on_terminated(Terminated { who, why }, &mut ctx).await;
                        }
                    }
                }
            }
            event = driver.next() => {
                match event {
                    Some(ev) => {
                        let result = driver.apply(&mut state, &mut ctx, ev).await;
                        timeout.set(timeout_fn());
                        if result.is_ok() {
                            continue;
                        }
                        match &supervision.strategy {
                            SupervisionStrategy::Resume => continue,
                            SupervisionStrategy::Stop => break TerminatedReason::Stopped,
                            SupervisionStrategy::Restart { max_retries, within } => {
                                if restart_tracker.should_restart(*max_retries, *within) {
                                    state.on_stop(&mut ctx).await;
                                    on_stop_called_in_restart = true;
                                    let deferred = stop_children_for_restart(
                                        &mut ctx,
                                        &mut sys_rx,
                                        &mut watchers,
                                    )
                                    .await;
                                    if let Some(abort_reason) = deferred.abort {
                                        // Stop/Poison arrived during the child-drain wait.
                                        // on_stop was already called above; the post-loop
                                        // must not call it again.
                                        break abort_reason;
                                    }
                                    state = (supervision.producer)();
                                    on_stop_called_in_restart = false;
                                    state.on_start(&mut ctx).await;
                                    for (who, why) in deferred.pending_terminated {
                                        state.on_terminated(Terminated { who, why }, &mut ctx).await;
                                    }
                                } else {
                                    break TerminatedReason::Stopped;
                                }
                            }
                        }
                    },
                    None => break TerminatedReason::Stopped,
                }
            }
            _ = &mut timeout => {
                break TerminatedReason::Timeout;
            }
        }
    };

    if reason != TerminatedReason::Poisoned && !on_stop_called_in_restart {
        state.on_stop(&mut ctx).await;
    }

    stop_children_and_wait(&mut ctx, &mut sys_rx, &mut watchers).await;

    registry.unregister(pid, process_name.as_ref()).await;

    // Drain any Watch signals that arrived after the last select! iteration
    // but before unregister, so we notify those watchers too.
    while let Ok(sig) = sys_rx.try_recv() {
        if let SystemSignal::Watch { watcher_pid } = sig {
            watchers.insert(watcher_pid);
        }
    }

    for watcher_pid in &watchers {
        if let Some(proxy) = registry.lookup(*watcher_pid).await {
            let _ = proxy
                .send_system_signal(SystemSignal::Terminated {
                    who: pid,
                    why: reason,
                })
                .await;
        }
    }

    // Always notify the parent (if any) so that `stop_children_and_wait`
    // can complete even when the parent has previously called `unwatch` on
    // this child.
    //
    // - Parent is in `watchers` (either from spawn_child's implicit Watch or from
    //   an explicit ctx.watch call): the watcher loop above already sent
    //   `Terminated`; skip the extra notification to avoid a duplicate.
    // - Parent is NOT in `watchers` (parent called ctx.unwatch): send `Terminated`
    //   via this fallback so the parent's stop_children_and_wait can still drain.
    if let Some(parent_pid) = init.parent {
        if !watchers.contains(&parent_pid) {
            if let Some(proxy) = registry.lookup(parent_pid).await {
                let _ = proxy
                    .send_system_signal(SystemSignal::Terminated { who: pid, why: reason })
                    .await;
            }
        }
    }
}

async fn send_stop_to_children<P: Process>(ctx: &mut ProcessContext<P>) {
    let child_pids: Vec<Pid> = ctx.children.iter_rev().copied().collect();

    for cpid in &child_pids {
        match ctx.registry.lookup(*cpid).await {
            Some(proxy) => {
                let _ = proxy.send_system_signal(SystemSignal::Stop).await;
            }
            None => {
                ctx.children.remove(cpid);
            }
        }
    }
}

/// Drain `ctx.children`: send `Stop` to each child (reverse order),
/// then block on `sys_rx` until every child has reported `Terminated`.
///
/// `Watch` / `Unwatch` signals arriving during the wait are recorded in
/// `watchers` so the eventual termination notification reaches them. The
/// user's `on_terminated` is **not** invoked here — `on_stop` has already run
/// and re-entering user code while the lifecycle is being torn down would
/// invalidate the user's reasoning about reentrancy.
async fn stop_children_and_wait<P: Process>(
    ctx: &mut ProcessContext<P>,
    sys_rx: &mut mpsc::Receiver<SystemSignal>,
    watchers: &mut HashSet<Pid>,
) {
    send_stop_to_children(ctx).await;

    while !ctx.children.is_empty() {
        match sys_rx.recv().await {
            Some(SystemSignal::Terminated { who, .. }) if ctx.children.contains(&who) => {
                ctx.children.remove(&who);
                ctx.explicit_watches.lock().unwrap().remove(&who);
            }
            Some(SystemSignal::Watch { watcher_pid }) => {
                watchers.insert(watcher_pid);
            }
            Some(SystemSignal::Unwatch { watcher_pid }) => {
                watchers.remove(&watcher_pid);
            }
            Some(_) => {}
            None => break,
        }
    }
}

/// Signals deferred during a supervision-restart child drain that must be
/// handled after the children have stopped.
struct RestartDeferred {
    /// `Some` if a `Stop` or `Poison` signal arrived while waiting — the
    /// restart should be aborted and the process should exit with this reason.
    abort: Option<TerminatedReason>,
    /// Non-child `Terminated` notifications received during the wait; replayed
    /// to the new process state after a successful restart.
    pending_terminated: Vec<(Pid, TerminatedReason)>,
}

/// Variant of `stop_children_and_wait` used during supervision restart.
///
/// Unlike the normal-termination path, this function preserves signals that
/// would otherwise be lost while waiting for children to stop:
///
/// - `Stop` / `Poison` → stored in [`RestartDeferred::abort`] so the caller
///   can abort the restart and propagate the reason.
/// - `Terminated` for an explicit-watch child → collected in
///   [`RestartDeferred::pending_terminated`] for replay to the new state.
/// - `Terminated` for a hierarchy-only child (not in explicit_watches) → bookkeeping
///   only; removed from `ctx.children` but not replayed.
/// - `Terminated` for an external death-watch (not a child) → collected for replay.
///
/// `on_stop` is intentionally **not** called here; the caller must invoke it
/// **before** calling this function (spec order: `on_stop` → stop children →
/// await `Terminated`).  The caller tracks whether `on_stop` was already called
/// so the post-loop cleanup does not call it a second time.
async fn stop_children_for_restart<P: Process>(
    ctx: &mut ProcessContext<P>,
    sys_rx: &mut mpsc::Receiver<SystemSignal>,
    watchers: &mut HashSet<Pid>,
) -> RestartDeferred {
    send_stop_to_children(ctx).await;

    let mut abort: Option<TerminatedReason> = None;
    let mut pending_terminated: Vec<(Pid, TerminatedReason)> = Vec::new();

    while !ctx.children.is_empty() {
        match sys_rx.recv().await {
            Some(SystemSignal::Terminated { who, why }) if ctx.children.contains(&who) => {
                ctx.children.remove(&who);
                let was_explicit = ctx.explicit_watches.lock().unwrap().remove(&who);
                if was_explicit {
                    pending_terminated.push((who, why));
                }
            }
            Some(SystemSignal::Terminated { who, why }) => {
                ctx.explicit_watches.lock().unwrap().remove(&who);
                pending_terminated.push((who, why));
            }
            Some(SystemSignal::Stop) => {
                if abort.is_none() {
                    abort = Some(TerminatedReason::Stopped);
                }
            }
            Some(SystemSignal::Poison) => {
                abort = Some(TerminatedReason::Poisoned);
            }
            Some(SystemSignal::Watch { watcher_pid }) => {
                watchers.insert(watcher_pid);
            }
            Some(SystemSignal::Unwatch { watcher_pid }) => {
                watchers.remove(&watcher_pid);
            }
            None => break,
        }
    }

    RestartDeferred {
        abort,
        pending_terminated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::time::Duration;

    use crate::error::HandlerError;
    use crate::process::props::SupervisionStrategy;
    use crate::process::supervision::SupervisionConfig;
    use crate::process::task::UserTask;

    struct NoOpProcess;
    impl Process for NoOpProcess {}

    struct PendingNeverIdleDriver;

    impl Driver<NoOpProcess> for PendingNeverIdleDriver {
        type Event = ();

        fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
            std::future::pending()
        }

        async fn apply(
            &mut self,
            _state: &mut NoOpProcess,
            _ctx: &mut ProcessContext<NoOpProcess>,
            _ev: (),
        ) -> Result<(), HandlerError> {
            Ok(())
        }

        fn supports_idle_timeout(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn lifecycle_loop_disarms_idle_timer_when_driver_opts_out() {
        let registry = ProcessRegistry::new();
        let pid = Pid::next();
        let (sys_tx, sys_rx) = mpsc::channel::<SystemSignal>(32);

        let watcher_pid = Pid::next();
        let (watcher_user_tx, _watcher_user_rx) = mpsc::channel::<UserTask<NoOpProcess>>(32);
        let (watcher_sys_tx, mut watcher_sys_rx) = mpsc::channel::<SystemSignal>(32);
        let watcher_proxy = ProcessProxy::<NoOpProcess> {
            pid: watcher_pid,
            user_tx: watcher_user_tx,
            sys_tx: watcher_sys_tx,
            dead_letter: None,
            registry: registry.clone(),
        };
        registry
            .register(watcher_pid, watcher_proxy.into(), None)
            .await;

        sys_tx
            .send(SystemSignal::Watch { watcher_pid })
            .await
            .unwrap();

        let supervision = SupervisionConfig {
            producer: Box::new(|| NoOpProcess),
            strategy: SupervisionStrategy::Stop,
        };

        let (self_user_tx, _self_user_rx) = mpsc::channel::<UserTask<NoOpProcess>>(32);
        let self_proxy = ProcessProxy::<NoOpProcess> {
            pid,
            user_tx: self_user_tx,
            sys_tx: sys_tx.clone(),
            dead_letter: None,
            registry: registry.clone(),
        };

        let loop_handle = tokio::spawn(lifecycle_loop(
            NoOpProcess,
            None,
            registry,
            pid,
            PendingNeverIdleDriver,
            sys_tx.clone(),
            sys_rx,
            Some(Duration::from_millis(50)),
            None,
            supervision,
            self_proxy,
            LifecycleInit { parent: None, default_idle_timeout: None },
        ));

        // Wait well past the configured 50 ms timeout — if the timer were still
        // armed, the loop would already be finished at this point.
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            !loop_handle.is_finished(),
            "lifecycle_loop must not exit due to idle timeout when \
             supports_idle_timeout() returns false"
        );

        sys_tx.send(SystemSignal::Stop).await.unwrap();
        loop_handle.await.expect("lifecycle_loop task panicked");

        let signal = watcher_sys_rx
            .recv()
            .await
            .expect("watcher must receive a Terminated signal after Stop");
        match signal {
            SystemSignal::Terminated { who, why } => {
                assert_eq!(who, pid, "Terminated.who must equal the target pid");
                assert_eq!(
                    why,
                    TerminatedReason::Stopped,
                    "expected TerminatedReason::Stopped, got {why:?}: \
                     idle timer must not have fired when supports_idle_timeout() is false"
                );
            }
            other => panic!("expected SystemSignal::Terminated, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // ARCH-REVIEW-004 regression: watch / unwatch must take &self, not &mut self.
    // If either method required exclusive access, this function would fail to
    // compile, catching the regression before any runtime test runs.
    // -----------------------------------------------------------------------
    #[allow(dead_code)]
    fn _assert_watch_unwatch_are_shared_ref(ctx: &ProcessContext<NoOpProcess>, pid: Pid) {
        // Calling these on a *shared* reference is the compile-time proof.
        let _w = ctx.watch(pid);
        let _u = ctx.unwatch(pid);
    }
}
