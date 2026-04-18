use std::time::Duration;
use futures_util::future::Either;
use crate::process::signal::SystemSignal;
use crate::process::{Process, ProcessContext, ProcessProxy, ProcessRegistry};
use tokio::sync::mpsc;
use crate::ident::{Pid, ProcessName};
use crate::process::task::UserTask;

pub(crate) async fn run<P: Process>(
    process: P,
    process_name: Option<ProcessName>,
    process_registry: ProcessRegistry,
    timeout: Option<Duration>
) -> ProcessProxy<P>
{
    let (user_tx, user_rx) = mpsc::channel::<UserTask<P>>(32);
    let (sys_tx, sys_rx) = mpsc::channel::<SystemSignal>(32);
    
    let pid = Pid::next();
    
    let proxy = ProcessProxy {
        pid,
        user_tx,
        sys_tx,
    };
    
    let ctx = ProcessContext {
        pid,
        name: process_name,
    };
    
    let fut = async move {
        let mut state = process;
        let mut user_rx = user_rx;
        let mut sys_rx = sys_rx;
        
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
        
        state.on_stop().await;
    };
    
    #[cfg(not(tokio_unstable))]
    tokio::spawn(fut);
    
    #[cfg(tokio_unstable)]
    {
        let _ = tokio::task::Builder::new()
            .name(named.as_ref())
            .spawn(process)
            .expect("unexpected error occurred from tokio-runtime.");
    }
    
    proxy
}
