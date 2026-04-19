use std::any::TypeId;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::error::BoxError;
use crate::ident::Pid;
use crate::process::dead_letter::DeadLetterEnvelope;
use crate::process::message::Boxed;
use crate::process::{Process, ProcessContext, Receive};

pub(crate) type UserTask<P> = Box<dyn Task<P>>;

#[async_trait]
pub(crate) trait Task<P: Process>: 'static + Sync + Send {
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext);
    fn into_dead_letter_envelope(
        self: Box<Self>,
        destination: Pid,
        sender: Option<Pid>,
        suppress_log: bool,
    ) -> DeadLetterEnvelope;
}

fn build_envelope<M: 'static + Send + Sync>(
    msg: M,
    destination: Pid,
    sender: Option<Pid>,
    suppress_log: bool,
) -> DeadLetterEnvelope {
    DeadLetterEnvelope {
        destination,
        message: Boxed::new(msg),
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
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext) {
        let _ = state.recv(self.msg, ctx).await;
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

pub(crate) struct AskTask<M, R> {
    msg: M,
    reply_tx: oneshot::Sender<Result<R, BoxError>>,
}

impl<M, R> AskTask<M, R> {
    pub fn new(msg: M, reply_tx: oneshot::Sender<Result<R, BoxError>>) -> Self {
        Self { msg, reply_tx }
    }
}

#[async_trait]
impl<P, M> Task<P> for AskTask<M, <P as Receive<M>>::Response>
where
    P: Receive<M>,
    M: 'static + Send + Sync,
    <P as Receive<M>>::Response: 'static + Send,
{
    async fn run(self: Box<Self>, state: &mut P, ctx: &mut ProcessContext) {
        let result = state.recv(self.msg, ctx).await;
        let _ = self.reply_tx.send(result);
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
