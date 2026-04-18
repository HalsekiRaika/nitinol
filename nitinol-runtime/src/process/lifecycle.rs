use std::time::Duration;

use futures_util::future::Either;
use tokio::sync::mpsc;

use crate::ident::{Pid, ProcessName};
use crate::process::signal::SystemSignal;
use crate::process::task::UserTask;
use crate::process::{Process, ProcessProxy, ProcessRegistry};

pub(crate) async fn run<P: Process>(
    process: P,
    process_name: Option<ProcessName>,
    registry: ProcessRegistry,
    timeout: Option<Duration>,
) -> ProcessProxy<P> {
    let (user_tx, user_rx) = mpsc::channel::<UserTask<P>>(32);
    let (sys_tx, sys_rx) = mpsc::channel::<SystemSignal>(32);

    let pid = Pid::next();

    let proxy = ProcessProxy {
        pid,
        user_tx,
        sys_tx,
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

    let fut = lifecycle_loop(process, process_name, registry, pid, user_rx, sys_rx, timeout);

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
    sys_rx: mpsc::Receiver<SystemSignal>,
    timeout: Option<Duration>,
) {
    let mut state = process;
    let mut user_rx = user_rx;
    let mut sys_rx = sys_rx;
    let mut poisoned = false;

    let timeout_fn = move || match timeout {
        Some(dur) => Either::Left(tokio::time::sleep(dur)),
        None => Either::Right(std::future::pending::<()>()),
    };

    tokio::pin! {
        let timeout = timeout_fn();
    }

    state.on_start().await;

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
                }
            }
            task = user_rx.recv() => {
                match task {
                    Some(task) => {
                        task.run(&mut state).await;
                        timeout.set(timeout_fn());
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
        state.on_stop().await;
    }

    registry.unregister(pid, process_name.as_ref()).await;
}
