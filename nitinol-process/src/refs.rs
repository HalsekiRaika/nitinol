use std::any::Any;
use std::error::Error;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use crate::channel::{ProcessApplier, PublishHandler, ApplicativeHandler, TryApplicativeHandler, NoCallBackApplicativeHandler, NoCallBackTryApplicativeHandler, EmployHandler};
use crate::errors::ChannelDropped;
use crate::{Applicator, Process, Publisher, TryApplicator};
use self::any::DynRef;

pub mod any;

#[derive(Debug)]
pub struct Ref<T: Process> {
    pub(crate) channel: UnboundedSender<Box<dyn ProcessApplier<T>>>
}


impl<T: Process> Ref<T> {
    #[rustfmt::skip]
    pub async fn publish<C: Command>(&self, command: C) -> Result<Result<T::Event, T::Rejection>, ChannelDropped>
    where
        T: Publisher<C>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(PublishHandler {
                command,
                oneshot: tx,
            }))
            .map_err(|_| ChannelDropped)?;

        rx.await.map_err(|_| ChannelDropped)
    }

    pub async fn apply<E: Event>(&self, event: E) -> Result<(), ChannelDropped>
    where
        T: Applicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(ApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| ChannelDropped)?;

        rx.await.map_err(|_| ChannelDropped)
    }
    
    pub async fn try_apply<E: Event>(&self, event: E) -> Result<Result<(), T::Rejection>, ChannelDropped>
    where
        T: TryApplicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(TryApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| ChannelDropped)?;
        
        rx.await.map_err(|_| ChannelDropped)
    }
    
    pub fn notify<E: Event>(&self, event: E) -> Result<(), ChannelDropped>
    where 
        T: Applicator<E>
    {
        self.channel
            .send(Box::new(NoCallBackApplicativeHandler { event }))
            .map_err(|_| ChannelDropped)
    }
    
    pub fn entrust<E: Event>(&self, event: E) -> Result<(), ChannelDropped> 
    where 
        T: TryApplicator<E>,
        T::Rejection: Error
    {
        self.channel
            .send(Box::new(NoCallBackTryApplicativeHandler { event }))
            .map_err(|_| ChannelDropped)
    }
    
    pub async fn employ<C: Command>(&self, cmd: C) -> Result<Result<(), T::Rejection>, ChannelDropped>
    where
        T: Publisher<C>,
        T: Applicator<<T as Publisher<C>>::Event>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(EmployHandler { command: cmd, channel: tx }))
            .map_err(|_| ChannelDropped)?;
        
        rx.await.map_err(|_| ChannelDropped)
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