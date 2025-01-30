use async_trait::async_trait;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use crate::{Applicator, Context, Process, ProcessApplier, Publisher};
use crate::errors::ChannelDropped;

pub struct EmployHandler<T: Process, C: Command>
where
    T: Publisher<C>,
    T: Applicator<<T as Publisher<C>>::Event>,
{
    pub(crate) command: C,
    pub(crate) channel: oneshot::Sender<Result<(), T::Rejection>>
}

#[async_trait]
impl<C: Command, T: Process> ProcessApplier<T> for EmployHandler<T, C> 
where
    T: Publisher<C>,
    T: Applicator<<T as Publisher<C>>::Event>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped> {
        match entity.publish(self.command, ctx).await {
            Ok(event) => {
                entity.apply(event, ctx).await;
                ctx.sequence += 1;
                self.channel.send(Ok(())).map_err(|_| ChannelDropped)
            }
            Err(rejection) => {
                self.channel.send(Err(rejection)).map_err(|_| ChannelDropped)
            }
        }
    }
}