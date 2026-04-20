mod any;

use std::any::Any;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

pub use self::any::*;

use crate::error::BoxError;
use crate::ident::Pid;
use crate::process::dead_letter::{suppress_log, DeadLetterProxy, DeadLetterResponse};
use crate::process::signal::SystemSignal;
use crate::process::task::{AskTask, TellTask, UserTask};
use crate::process::{Process, Receive};

pub struct ProcessProxy<P> {
    pub(crate) pid: Pid,
    pub(crate) user_tx: mpsc::Sender<UserTask<P>>,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
}

impl<P> Clone for ProcessProxy<P> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            user_tx: self.user_tx.clone(),
            sys_tx: self.sys_tx.clone(),
            dead_letter: self.dead_letter.clone(),
        }
    }
}

impl<P: Process> ProcessProxy<P> {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    async fn route_to_dead_letter(&self, task: UserTask<P>, suppress: bool) {
        if let Some(ref dl) = self.dead_letter {
            let envelope = task.into_dead_letter_envelope(self.pid, None, suppress);
            dl.send(envelope).await;
        }
    }

    pub async fn tell<M>(&self, msg: M) -> Result<(), BoxError>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
    {
        let suppress = suppress_log::<M>();
        let task: UserTask<P> = Box::new(TellTask::new(msg));
        match self.user_tx.send(task).await {
            Ok(()) => Ok(()),
            Err(send_err) => {
                self.route_to_dead_letter(send_err.0, suppress).await;
                Err("process already stopped".into())
            }
        }
    }

    pub async fn ask<M>(&self, msg: M) -> Result<<P as Receive<M>>::Response, BoxError>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
        <P as Receive<M>>::Response: 'static + Send,
    {
        let suppress = suppress_log::<M>();
        let (tx, rx) = oneshot::channel();
        let task: UserTask<P> = Box::new(AskTask::new(msg, tx));
        if let Err(send_err) = self.user_tx.send(task).await {
            self.route_to_dead_letter(send_err.0, suppress).await;
            return Err(Box::new(DeadLetterResponse {
                destination: self.pid,
            }));
        }
        rx.await
            .map_err(|_| -> BoxError { "process dropped reply".into() })?
    }

    pub async fn stop(&self) -> Result<(), BoxError> {
        self.sys_tx
            .send(SystemSignal::Stop)
            .await
            .map_err(|_| -> BoxError { "process already stopped".into() })
    }

    pub async fn poison(&self) -> Result<(), BoxError> {
        self.sys_tx
            .send(SystemSignal::Poison)
            .await
            .map_err(|_| -> BoxError { "process already stopped".into() })
    }
}

#[async_trait]
impl<P> DynProxy for ProcessProxy<P>
where
    P: Process,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn send_sys_sig(&self, signal: SystemSignal) -> Result<(), BoxError> {
        self.sys_tx
            .send(signal)
            .await
            .map_err(|_| -> BoxError { "process already stopped".into() })
    }
}
