mod any;
mod temp_ask;

use std::any::Any;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

pub use self::any::*;

use crate::error::{AskError, SendError};
use crate::ident::Pid;
use crate::process::dead_letter::{self, DeadLetterProxy};
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::task::{AskTask, TellTask, UserTask};
use crate::process::{Process, Receive};

/// A cloneable handle to a running process of type `P`.
///
/// [`tell`](Self::tell) returns once the message is in the process's mailbox
/// and gives no reply, while [`ask`](Self::ask) waits for the handler's
/// [`Response`](Receive::Response).
pub struct ProcessProxy<P> {
    pub(crate) pid: Pid,
    pub(crate) user_tx: mpsc::Sender<UserTask<P>>,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
    pub(crate) dead_letter: Option<DeadLetterProxy>,
    pub(crate) registry: ProcessRegistry,
}

impl<P> Clone for ProcessProxy<P> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            user_tx: self.user_tx.clone(),
            sys_tx: self.sys_tx.clone(),
            dead_letter: self.dead_letter.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<P> ProcessProxy<P> {
    /// Requests that the process stop without waiting. Does nothing if the
    /// request cannot be sent.
    pub fn signal_stop_nonblocking(&self) {
        let _ = self.sys_tx.try_send(SystemSignal::Stop);
    }
}

impl<P: Process> ProcessProxy<P> {
    /// Returns the pid of the process.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    async fn route_to_dead_letter(&self, task: UserTask<P>, suppress: bool) {
        if let Some(ref dl) = self.dead_letter {
            let envelope = task.into_dead_letter_envelope(self.pid, None, suppress);
            dl.send(envelope).await;
        }
    }

    /// Sends `msg` to the process without waiting for a reply.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if the process has stopped and its mailbox is
    /// closed. The message is sent to the dead letters.
    pub async fn tell<M>(&self, msg: M) -> Result<(), SendError>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
    {
        let suppress = dead_letter::suppress_log::<M>();
        let task: UserTask<P> = Box::new(TellTask::new(msg));
        match self.user_tx.send(task).await {
            Ok(()) => Ok(()),
            Err(send_err) => {
                self.route_to_dead_letter(send_err.0, suppress).await;
                Err(SendError)
            }
        }
    }

    /// Sends `msg` to the process and waits for its reply.
    ///
    /// # Errors
    ///
    /// - [`AskError::DeadLetter`] if the process stopped before the message
    ///   was delivered.
    /// - [`AskError::Handler`] if the handler returned `Err`.
    /// - [`AskError::ReplyDropped`] if the process stopped, restarted or
    ///   panicked before replying, or its stash was full.
    pub async fn ask<M>(
        &self,
        msg: M,
    ) -> Result<<P as Receive<M>>::Response, AskError<<P as Receive<M>>::Error>>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
        <P as Receive<M>>::Response: 'static + Send,
        <P as Receive<M>>::Error: 'static + Send,
    {
        let suppress = dead_letter::suppress_log::<M>();
        let (tx, rx) =
            oneshot::channel::<Result<<P as Receive<M>>::Response, <P as Receive<M>>::Error>>();
        let task: UserTask<P> = Box::new(AskTask::new(msg, tx));
        if let Err(send_err) = self.user_tx.send(task).await {
            self.route_to_dead_letter(send_err.0, suppress).await;
            return Err(AskError::DeadLetter {
                destination: self.pid,
            });
        }
        match rx.await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(AskError::Handler(e)),
            Err(_) => Err(AskError::ReplyDropped),
        }
    }

    /// Send `msg` and wait up to `duration` for a reply.
    ///
    /// # Errors
    ///
    /// - [`AskError::Timeout`] if no reply arrives within `duration`.
    /// - [`AskError::DeadLetter`] if the process stopped before the message
    ///   was delivered.
    /// - [`AskError::Handler`] if the handler returned `Err`.
    /// - [`AskError::ReplyDropped`] if the process stopped, restarted or
    ///   panicked before replying, or its stash was full.
    pub async fn ask_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<<P as Receive<M>>::Response, AskError<<P as Receive<M>>::Error>>
    where
        P: Receive<M>,
        M: 'static + Send + Sync,
        <P as Receive<M>>::Response: 'static + Send,
        <P as Receive<M>>::Error: 'static + Send,
    {
        let suppress = dead_letter::suppress_log::<M>();
        let (in_tx, in_rx) =
            oneshot::channel::<Result<<P as Receive<M>>::Response, <P as Receive<M>>::Error>>();
        let (out_tx, out_rx) = oneshot::channel::<
            Result<<P as Receive<M>>::Response, AskError<<P as Receive<M>>::Error>>,
        >();
        let task: UserTask<P> = Box::new(AskTask::new(msg, in_tx));
        if let Err(send_err) = self.user_tx.send(task).await {
            self.route_to_dead_letter(send_err.0, suppress).await;
            return Err(AskError::DeadLetter {
                destination: self.pid,
            });
        }
        temp_ask::spawn_temp_ask(
            self.registry.clone(),
            self.dead_letter.clone(),
            self.pid,
            in_rx,
            out_tx,
            duration,
        )
        .await;
        match out_rx.await {
            Ok(result) => result,
            Err(_) => Err(AskError::ReplyDropped),
        }
    }

    /// Requests that the process stop. Its [`on_stop`](Process::on_stop) is
    /// called.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if the process has already stopped.
    pub async fn stop(&self) -> Result<(), SendError> {
        self.sys_tx
            .send(SystemSignal::Stop)
            .await
            .map_err(|_| SendError)
    }

    /// Stops the process without calling its [`on_stop`](Process::on_stop).
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if the process has already stopped.
    pub async fn poison(&self) -> Result<(), SendError> {
        self.sys_tx
            .send(SystemSignal::Poison)
            .await
            .map_err(|_| SendError)
    }
}

#[async_trait]
impl<P: Process> DynProxy for ProcessProxy<P> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn send_sys_sig(&self, signal: SystemSignal) -> Result<(), SendError> {
        self.sys_tx.send(signal).await.map_err(|_| SendError)
    }
}
