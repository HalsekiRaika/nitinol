use std::any::TypeId;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::error::HandlerError;
use crate::ident::Pid;
use crate::process::dead_letter::DeadLetterEnvelope;
use crate::process::message::BoxedMessage;
use crate::process::{Process, ProcessContext, Receive};

pub(crate) type UserTask<P> = Box<dyn Task<P>>;

#[async_trait]
pub(crate) trait Task<P: Process>: 'static + Sync + Send {
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext) -> Result<(), HandlerError>;
    fn into_dead_letter_envelope(
        self: Box<Self>,
        destination: Pid,
        sender: Option<Pid>,
        suppress_log: bool,
    ) -> DeadLetterEnvelope;
}

fn build_envelope<M>(
    msg: M,
    destination: Pid,
    sender: Option<Pid>,
    suppress_log: bool,
) -> DeadLetterEnvelope
where
    M: 'static + Send + Sync,
{
    DeadLetterEnvelope {
        destination,
        message: BoxedMessage::new(msg),
        sender,
        suppress_log,
        message_type_id: TypeId::of::<M>(),
    }
}

pub(crate) struct TellTask<M> {
    msg: M,
}

impl<M> TellTask<M> {
    pub fn new(msg: M) -> Self {
        Self { msg }
    }
}

#[async_trait]
impl<P, M> Task<P> for TellTask<M>
where
    P: Receive<M>,
    M: 'static + Send + Sync,
{
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext) -> Result<(), HandlerError> {
        state.recv(self.msg, ctx).await.map(|_| ()).map_err(|_| HandlerError)
    }

    fn into_dead_letter_envelope(
        self: Box<Self>,
        destination: Pid,
        sender: Option<Pid>,
        suppress_log: bool,
    ) -> DeadLetterEnvelope {
        build_envelope(self.msg, destination, sender, suppress_log)
    }
}

pub(crate) struct AskTask<M, R, E> {
    msg: M,
    reply_tx: oneshot::Sender<Result<R, E>>,
}

impl<M, R, E> AskTask<M, R, E> {
    pub fn new(msg: M, reply_tx: oneshot::Sender<Result<R, E>>) -> Self {
        Self { msg, reply_tx }
    }
}

#[async_trait]
impl<P, M> Task<P> for AskTask<M, <P as Receive<M>>::Response, <P as Receive<M>>::Error>
where
    P: Receive<M>,
    M: 'static + Send + Sync,
    <P as Receive<M>>::Response: 'static + Send,
    <P as Receive<M>>::Error: 'static + Send,
{
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext) -> Result<(), HandlerError> {
        let result = state.recv(self.msg, ctx).await;
        let failed = result.is_err();
        let _ = self.reply_tx.send(result);
        // Propagate failure to the lifecycle loop for supervision.
        if failed {
            Err(HandlerError)
        } else {
            Ok(())
        }
    }

    fn into_dead_letter_envelope(
        self: Box<Self>,
        destination: Pid,
        sender: Option<Pid>,
        suppress_log: bool,
    ) -> DeadLetterEnvelope {
        build_envelope(self.msg, destination, sender, suppress_log)
    }
}
