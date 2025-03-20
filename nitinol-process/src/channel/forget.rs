use std::fmt::Debug;

use async_trait::async_trait;
use nitinol_core::command::Command;
use crate::errors::ChannelDropped;
use crate::{Context, EventApplicator, Process, ProcessApplier, CommandHandler};

pub struct NonBlockingEntrustHandler<C: Command> {
    pub(crate) command: C,
}

#[async_trait]
impl<C: Command, T: Process> ProcessApplier<T> for NonBlockingEntrustHandler<C> 
where
    T: CommandHandler<C>,
    T::Rejection: Debug,
    T: EventApplicator<<T as CommandHandler<C>>::Event>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        match entity.handle(self.command, ctx).await {
            Ok(event) => {
                entity.apply(event, ctx).await;
                ctx.sequence += 1;
            }
            Err(rejection) => {
                tracing::error!("An error occurred: {:?}", rejection);
            }
        }
        Ok(())
    }
}
