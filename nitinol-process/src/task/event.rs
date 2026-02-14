use super::TaskApplier;
use crate::{Process, Context};
use async_trait::async_trait;
use nitinol_core::event::Event;
use tokio::sync::oneshot;
use crate::errors::ChannelDropped;

#[async_trait]
pub trait EventApplicator<E: Event>: 'static + Sync + Send {
    async fn apply(&mut self, event: E, ctx: &mut Context);
}

pub(crate) struct EventApplicatorTask<E: Event> {
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<()>,
}

#[async_trait]
impl<E: Event, T: Process> TaskApplier<T> for EventApplicatorTask<E>
where
    T: EventApplicator<E>,
{
    async fn apply(self: Box<Self>, state: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        self.oneshot
            .send(state.apply(self.event, ctx).await)
            .map_err(|_| ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}
