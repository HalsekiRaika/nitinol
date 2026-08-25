use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::AskError;
use crate::ident::Pid;
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::props::{IdleTimeout, Props};
use crate::process::registry::ProcessRegistry;
use crate::process::spawn::SpawnEnv;
use crate::process::{Process, ProcessContext, Receive};

/// Single-use handoff of the caller's `ask` receiver into the temp process.
type ReplyInSlot<R, E> = Mutex<Option<oneshot::Receiver<Result<R, E>>>>;

/// Single-use handoff of the caller's reply sender into the temp process.
type ReplyOutSlot<R, E> = Mutex<Option<oneshot::Sender<Result<R, AskError<E>>>>>;

pub(crate) struct ReplyArrived<R, E> {
    payload: Mutex<Option<Result<R, E>>>,
}

impl<R, E> ReplyArrived<R, E> {
    fn new(payload: Option<Result<R, E>>) -> Self {
        Self {
            payload: Mutex::new(payload),
        }
    }

    fn take(&self) -> Option<Result<R, E>> {
        self.payload
            .lock()
            .expect("reply payload lock was poisoned by a panicking holder")
            .take()
    }
}

pub(crate) struct TempAskProcess<R, E>
where
    R: 'static + Send,
    E: 'static + Send + std::error::Error,
{
    out_tx: Option<oneshot::Sender<Result<R, AskError<E>>>>,
    in_rx: Option<oneshot::Receiver<Result<R, E>>>,
    destination: Pid,
}

impl<R, E> Process for TempAskProcess<R, E>
where
    R: 'static + Send,
    E: 'static + Send + Sync + std::error::Error,
{
    fn on_start(
        &mut self,
        ctx: &mut ProcessContext<Self>,
    ) -> impl std::future::Future<Output = ()> + Send {
        let in_rx = self.in_rx.take();
        async move {
            if let Some(rx) = in_rx {
                ctx.pipe_to_self(rx, |outcome| match outcome {
                    Ok(Ok(reply)) => ReplyArrived::<R, E>::new(Some(reply)),
                    Ok(Err(_)) | Err(_) => ReplyArrived::<R, E>::new(None),
                });
            }
        }
    }

    fn on_stop(
        &mut self,
        _ctx: &mut ProcessContext<Self>,
    ) -> impl std::future::Future<Output = ()> + Send {
        let out_tx = self.out_tx.take();
        let destination = self.destination;
        async move {
            if let Some(tx) = out_tx {
                let _ = tx.send(Err(AskError::Timeout { destination }));
            }
        }
    }
}

impl<R, E> Receive<ReplyArrived<R, E>> for TempAskProcess<R, E>
where
    R: 'static + Send,
    E: 'static + Send + Sync + std::error::Error,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: ReplyArrived<R, E>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        if let Some(tx) = self.out_tx.take() {
            let payload = match msg.take() {
                Some(Ok(r)) => Ok(r),
                Some(Err(e)) => Err(AskError::Handler(e)),
                None => Err(AskError::ReplyDropped),
            };
            let _ = tx.send(payload);
        }
        let _ = ctx.stop_self().await;
        Ok(())
    }
}

pub(crate) async fn spawn_temp_ask<R, E>(
    registry: ProcessRegistry,
    dead_letter: Option<DeadLetterProxy>,
    destination: Pid,
    in_rx: oneshot::Receiver<Result<R, E>>,
    out_tx: oneshot::Sender<Result<R, AskError<E>>>,
    duration: Duration,
) where
    R: 'static + Send,
    E: 'static + Send + Sync + std::error::Error,
{
    let in_rx_slot: ReplyInSlot<R, E> = Mutex::new(Some(in_rx));
    let out_tx_slot: ReplyOutSlot<R, E> = Mutex::new(Some(out_tx));
    let producer = move || TempAskProcess {
        out_tx: out_tx_slot
            .lock()
            .expect("reply sender slot lock was poisoned by a panicking holder")
            .take(),
        in_rx: in_rx_slot
            .lock()
            .expect("reply receiver slot lock was poisoned by a panicking holder")
            .take(),
        destination,
    };
    let props: Props<TempAskProcess<R, E>> =
        Props::new(producer).with_idle_timeout(IdleTimeout::After(duration));
    let _ = SpawnEnv::detached(registry, dead_letter).spawn(props).await;
}
