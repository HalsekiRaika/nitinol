use async_trait::async_trait;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use nitinol_core::resolver::ResolveMapping;
use super::Applier;
use crate::Context;
use crate::errors::AgentError;

#[async_trait]
pub trait Publisher<C: Command>: 'static + Sync + Send {
    type Event: Event;
    type Rejection: 'static + Sync + Send;
    async fn publish(&self, command: C, ctx: &mut Context) -> Result<Self::Event, Self::Rejection>;
}

pub(crate) struct PublishHandler<C: Command, T: ResolveMapping>
where
    T: Publisher<C>,
{
    pub(crate) command: C,
    pub(crate) oneshot: oneshot::Sender<Result<T::Event, T::Rejection>>,
}

#[async_trait::async_trait]
impl<C: Command, T: ResolveMapping> Applier<T> for PublishHandler<C, T>
where
    T: Publisher<C>,
{
    async fn apply(
        self: Box<Self>,
        entity: &mut T,
        ctx: &mut Context,
    ) -> Result<(), AgentError> {
        self.oneshot
            .send(entity.publish(self.command, ctx).await)
            .map_err(|_| AgentError::ChannelDropped)
    }
}
