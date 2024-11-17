use crate::agent::errors::AgentError;
use crate::agent::{Applier, Context};
use crate::mapping::ResolveMapping;
use crate::Event;
use async_trait::async_trait;
use tokio::sync::oneshot;

#[async_trait]
pub trait Applicator<E: Event>: 'static + Sync + Send {
    async fn apply(&mut self, event: E, ctx: &mut Context);
}

pub(crate) struct ApplicativeHandler<E: Event> {
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<()>,
}

#[async_trait::async_trait]
impl<E: Event, T: ResolveMapping> Applier<T> for ApplicativeHandler<E>
where
    T: Applicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), AgentError> {
        self.oneshot
            .send(entity.apply(self.event, ctx).await)
            .map_err(|_| AgentError::ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}
