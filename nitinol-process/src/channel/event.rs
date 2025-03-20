use super::ProcessApplier;
use crate::{Process, Context};
use async_trait::async_trait;
use nitinol_core::event::Event;
use tokio::sync::oneshot;
use crate::errors::ChannelDropped;

#[async_trait]
pub trait EventApplicator<E: Event>: 'static + Sync + Send {
    async fn apply(&mut self, event: E, ctx: &mut Context);
}

pub(crate) struct ApplicativeReceptor<E: Event> {
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<()>,
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for ApplicativeReceptor<E>
where
    T: EventApplicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        self.oneshot
            .send(entity.apply(self.event, ctx).await)
            .map_err(|_| ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}


#[async_trait]
pub trait TryEventApplicator<E: Event>: 'static + Sync + Send {
    type Rejection: 'static + Sync + Send;
    async fn try_apply(&mut self, event: E, ctx: &mut Context) -> Result<(), Self::Rejection>;
}

pub(crate) struct TryApplicativeReceptor<E: Event, T: Process>
where
    T: TryEventApplicator<E>,
{
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<Result<(), T::Rejection>>,
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for TryApplicativeReceptor<E, T>
where
    T: TryEventApplicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        self.oneshot
            .send(entity.try_apply(self.event, ctx).await)
            .map_err(|_| ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}
