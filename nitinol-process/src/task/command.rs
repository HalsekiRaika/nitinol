use std::fmt::Debug;
use super::TaskApplier;
use crate::errors::ChannelDropped;
use crate::{Process, Context};
use async_trait::async_trait;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use tokio::sync::oneshot;

#[async_trait]
pub trait CommandHandler<C: Command>: 'static + Sync + Send {
    type Event: Event;
    type Rejection: Debug + 'static + Sync + Send;
    async fn handle(&self, command: C, ctx: &mut Context) -> Result<Self::Event, Self::Rejection>;
}

pub(crate) struct CommandHandleTask<C: Command, T: Process>
where
    T: CommandHandler<C>,
{
    pub(crate) command: C,
    pub(crate) oneshot: oneshot::Sender<Result<T::Event, T::Rejection>>,
}

#[async_trait::async_trait]
impl<C: Command, T: Process> TaskApplier<T> for CommandHandleTask<C, T>
where
    T: CommandHandler<C>,
{
    async fn apply(self: Box<Self>, state: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        self.oneshot
            .send(state.handle(self.command, ctx).await)
            .map_err(|_| ChannelDropped)?;
        Ok(())
    }
}
