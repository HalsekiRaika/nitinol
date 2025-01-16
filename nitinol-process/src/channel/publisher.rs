use super::ProcessApplier;
use crate::errors::ChannelDropped;
use crate::{Process, Context};
use async_trait::async_trait;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use tokio::sync::oneshot;

#[async_trait]
pub trait Publisher<C: Command>: 'static + Sync + Send {
    type Event: Event;
    type Rejection: 'static + Sync + Send;
    async fn publish(&self, command: C, ctx: &mut Context) -> Result<Self::Event, Self::Rejection>;
}

pub(crate) struct PublishHandler<C: Command, T: Process>
where
    T: Publisher<C>,
{
    pub(crate) command: C,
    pub(crate) oneshot: oneshot::Sender<Result<T::Event, T::Rejection>>,
}

#[async_trait::async_trait]
impl<C: Command, T: Process> ProcessApplier<T> for PublishHandler<C, T>
where
    T: Publisher<C>,
{
    async fn apply(
        self: Box<Self>,
        entity: &mut T,
        ctx: &mut Context,
    ) -> Result<(), ChannelDropped> {
        self.oneshot
            .send(entity.publish(self.command, ctx).await)
            .map_err(|_| ChannelDropped)
    }
}
