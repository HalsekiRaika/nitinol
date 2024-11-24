use std::error::Error;
use super::ProcessApplier;
use crate::errors::ProcessError;
use crate::{Process, Context};
use async_trait::async_trait;
use nitinol_core::event::Event;
use tokio::sync::oneshot;

#[async_trait]
pub trait Applicator<E: Event>: 'static + Sync + Send {
    async fn apply(&mut self, event: E, ctx: &mut Context);
}

pub(crate) struct ApplicativeHandler<E: Event> {
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<()>,
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for ApplicativeHandler<E>
where
    T: Applicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ProcessError> {
        self.oneshot
            .send(entity.apply(self.event, ctx).await)
            .map_err(|_| ProcessError::ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}

pub(crate) struct NoCallBackApplicativeHandler<E: Event> {
    pub(crate) event: E
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for NoCallBackApplicativeHandler<E> 
where
    T: Applicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ProcessError> {
        entity.apply(self.event, ctx).await;
        ctx.sequence += 1;
        Ok(())
    }
}

#[async_trait]
pub trait TryApplicator<E: Event>: 'static + Sync + Send {
    type Rejection: 'static + Sync + Send;
    async fn try_apply(&mut self, event: E, ctx: &mut Context) -> Result<(), Self::Rejection>;
}

pub(crate) struct TryApplicativeHandler<E: Event, T: Process>
where
    T: TryApplicator<E>,
{
    pub(crate) event: E,
    pub(crate) oneshot: oneshot::Sender<Result<(), T::Rejection>>,
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for TryApplicativeHandler<E, T>
where
    T: TryApplicator<E>,
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ProcessError> {
        self.oneshot
            .send(entity.try_apply(self.event, ctx).await)
            .map_err(|_| ProcessError::ChannelDropped)?;
        ctx.sequence += 1;
        Ok(())
    }
}

pub(crate) struct NoCallBackTryApplicativeHandler<E: Event> {
    pub(crate) event: E
}

#[async_trait]
impl<E: Event, T: Process> ProcessApplier<T> for NoCallBackTryApplicativeHandler<E> 
where 
    T: TryApplicator<E>,
    T::Rejection: Error
{
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ProcessError> {
        if let Err(e) = entity.try_apply(self.event, ctx).await { 
            tracing::error!("{}", e);
        }
        ctx.sequence += 1;
        Ok(())
    }
}
