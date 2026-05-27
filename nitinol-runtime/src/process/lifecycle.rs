use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::Either;
use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::driver::{Driver, MessageDriver};
use crate::process::props::SupervisionStrategy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::supervision::{RestartTracker, SupervisionConfig};
use crate::process::task::UserTask;
use crate::process::watch::{Terminated, TerminatedReason};
use crate::process::{Process, ProcessContext, ProcessProxy};

pub(crate) async fn run<P: Process>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    timeout: Option<Duration>,
    dead_letter: Option<DeadLetterProxy>,
    supervision: SupervisionConfig<P>,
) -> ProcessProxy<P> {
    let (user_tx, user_rx) = mpsc::channel::<UserTask<P>>(32);
    let driver = MessageDriver::new(user_rx);
    run_with_driver(
        process,
        process_name,
        registry,
        user_tx,
        driver,
        timeout,
        dead_letter,
        supervision,
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
) -> ProcessProxy<P> {
    let (sys_tx, sys_rx) = mpsc::channel::<SystemSignal>(32);

    let pid = Pid::next();

    let proxy = ProcessProxy {
        pid,
        user_tx,
        sys_tx: sys_tx.clone(),
        dead_letter: dead_letter.clone(),
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
) {
    let mut state = process;
    let mut driver = driver;
    let mut sys_rx = sys_rx;
    let mut watchers: HashSet<Pid> = HashSet::new();
    let mut restart_tracker = RestartTracker::new();

    let mut ctx = ProcessContext {
        pid,
        name: process_name.clone(),
        registry: registry.clone(),
        sys_tx: sys_tx.clone(),
        dead_letter: dead_letter.clone(),
    };

    // A driver that opts out (e.g. tick / poll sources) has no meaningful
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
                        state.on_terminated(Terminated { who, why }, &mut ctx).await;
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
                                    state = (supervision.producer)();
                                    state.on_start(&mut ctx).await;
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

    if reason != TerminatedReason::Poisoned {
        state.on_stop(&mut ctx).await;
    }

    registry.unregister(pid, process_name.as_ref()).await;

    // Drain any Watch signals that arrived after the last select! iteration
    // but before unregister, so we notify those watchers too.
    while let Ok(sig) = sys_rx.try_recv() {
        if let SystemSignal::Watch { watcher_pid } = sig {
            watchers.insert(watcher_pid);
        }
    }

    for watcher_pid in watchers {
        if let Some(proxy) = registry.lookup(watcher_pid).await {
            let _ = proxy
                .send_system_signal(SystemSignal::Terminated {
                    who: pid,
                    why: reason,
                })
                .await;
        }
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
            _ctx: &mut ProcessContext,
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
}
