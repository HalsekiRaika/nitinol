use std::fmt::Debug;
use async_trait::async_trait;
use crate::{Context, Process};
use crate::errors::ChannelDropped;
use crate::message::Message;
use crate::task::TaskApplier;

#[async_trait]
pub trait Receive<M: Message>: 'static + Sync + Send {
    type Error: Debug;
    async fn receive(&mut self, message: M, ctx: &mut Context) -> Result<(), Self::Error>;
}

pub(crate) struct ReceiveTask<M> 
where
    M: Send + Sync + 'static
{
    pub(crate) message: M
}

#[async_trait]
impl<T: Process, M> TaskApplier<T> for ReceiveTask<M> 
where
    T: Receive<M>,
    M: Message
{
    async fn apply(self: Box<Self>, state: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        // FIXME: Improve error handling. :/
        state.receive(self.message, ctx).await.unwrap();
        Ok(())
    }
}
