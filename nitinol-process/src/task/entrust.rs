use std::fmt::Debug;

use async_trait::async_trait;
use nitinol_core::command::Command;
use crate::errors::ChannelDropped;
use crate::{Context, Process};
use crate::task::{CommandHandler, EventApplicator, TaskApplier};

pub struct EntrustTask<C: Command> {
    pub(crate) command: C,
}

#[async_trait]
impl<C: Command, T: Process> TaskApplier<T> for EntrustTask<C> 
where
    T: CommandHandler<C>,
    T::Rejection: Debug,
    T: EventApplicator<<T as CommandHandler<C>>::Event>,
{
    async fn apply(self: Box<Self>, state: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        match state.handle(self.command, ctx).await {
            Ok(event) => {
                state.apply(event, ctx).await;
                ctx.sequence += 1;
            }
            Err(rejection) => {
                tracing::error!("An error occurred: {:?}", rejection);
            }
        }
        Ok(())
    }
}
