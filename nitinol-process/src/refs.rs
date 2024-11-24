use std::any::Any;
use std::error::Error;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use crate::channel::{
    ProcessApplier,
    PublishHandler,
    ApplicativeHandler,
    TryApplicativeHandler,
    NoCallBackApplicativeHandler,
    NoCallBackTryApplicativeHandler,
};
use crate::errors::ProcessError;
use crate::{Applicator, Process, Publisher, TryApplicator};
use self::any::DynRef;

pub mod any;

#[derive(Debug)]
pub struct Ref<T: Process> {
    pub(crate) channel: UnboundedSender<Box<dyn ProcessApplier<T>>>
}


impl<T: Process> Ref<T> {
    #[rustfmt::skip]
    pub async fn publish<C: Command>(&self, command: C) -> Result<Result<T::Event, T::Rejection>, ProcessError>
    where
        T: Publisher<C>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(PublishHandler {
                command,
                oneshot: tx,
            }))
            .map_err(|_| ProcessError::ChannelDropped)?;

        rx.await.map_err(|_| ProcessError::ChannelDropped)
    }

    pub async fn apply<E: Event>(&self, event: E) -> Result<(), ProcessError>
    where
        T: Applicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(ApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| ProcessError::ChannelDropped)?;

        rx.await.map_err(|_| ProcessError::ChannelDropped)
    }
    
    pub async fn try_apply<E: Event>(&self, event: E) -> Result<Result<(), T::Rejection>, ProcessError>
    where
        T: TryApplicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(TryApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| ProcessError::ChannelDropped)?;
        
        rx.await.map_err(|_| ProcessError::ChannelDropped)
    }
    
    pub fn notify<E: Event>(&self, event: E) -> Result<(), ProcessError>
    where 
        T: Applicator<E>
    {
        self.channel
            .send(Box::new(NoCallBackApplicativeHandler { event }))
            .map_err(|_| ProcessError::ChannelDropped)
    }
    
    pub fn entrust<E: Event>(&self, event: E) -> Result<(), ProcessError> 
    where 
        T: TryApplicator<E>,
        T::Rejection: Error
    {
        self.channel
            .send(Box::new(NoCallBackTryApplicativeHandler { event }))
            .map_err(|_| ProcessError::ChannelDropped)
    }
}

impl<T: Process> Clone for Ref<T> {
    fn clone(&self) -> Self {
        Self { 
            channel: self.channel.clone()
        }
    }
}

impl<T: Process> DynRef for Ref<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}