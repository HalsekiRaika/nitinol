use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::Either;
use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterRef;
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
    dead_letter: Option<DeadLetterRef>,
    supervision: Option<SupervisionConfig<P>>,
) -> ProcessProxy<P> {
    let (user_tx, user_rx) = mpsc::channel::<UserTask<P>>(32);
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
        user_rx,
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

async fn lifecycle_loop<P: Process>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    pid: Pid,
    user_rx: mpsc::Receiver<UserTask<P>>,
    sys_tx: mpsc::Sender<SystemSignal>,
    sys_rx: mpsc::Receiver<SystemSignal>,
    timeout: Option<Duration>,
    dead_letter: Option<DeadLetterRef>,
    supervision: Option<SupervisionConfig<P>>,
) {
    let mut state = process;
    let mut user_rx = user_rx;
    let mut sys_rx = sys_rx;
    let mut poisoned = false;
    let mut watchers: HashSet<Pid> = HashSet::new();
    let mut restart_tracker = RestartTracker::new();

    let mut ctx = ProcessContext {
        pid,
        name: process_name.clone(),
        registry: registry.clone(),
        sys_tx: sys_tx.clone(),
        dead_letter: dead_letter.clone(),
    };

    let timeout_fn = move || match timeout {
        Some(dur) => Either::Left(tokio::time::sleep(dur)),
        None => Either::Right(std::future::pending::<()>()),
    };

    tokio::pin! {
        let timeout = timeout_fn();
    }

    state.on_start(&mut ctx).await;

    loop {
        tokio::select! {
            biased;
            Some(sys_sig) = sys_rx.recv() => {
                match sys_sig {
                    SystemSignal::Stop => break,
                    SystemSignal::Poison => {
                        poisoned = true;
                        break;
                    }
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
            task = user_rx.recv() => {
                match task {
                    Some(task) => {
                        let result = task.run(&mut state, &mut ctx).await;
                        timeout.set(timeout_fn());
                        if result.is_ok() {
                            continue;
                        }
                        let config = match &supervision {
                            Some(c) => c,
                            None => continue, // No supervision (built-in processes): ignore handler errors.
                        };
                        match &config.strategy {
                            SupervisionStrategy::Stop => break,
                            SupervisionStrategy::Restart { max_retries, within } => {
                                if restart_tracker.should_restart(*max_retries, *within) {
                                    state.on_stop(&mut ctx).await;
                                    state = (config.producer)();
                                    state.on_start(&mut ctx).await;
                                    // Loop continues; watchers set is preserved.
                                } else {
                                    break;
                                }
                            }
                        }
                    },
                    None => break,
                }
            }
            _ = &mut timeout => {
                break;
            }
        }
    }

    if !poisoned {
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
                    why: TerminatedReason::Stopped,
                })
                .await;
        }
    }
}
