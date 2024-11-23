use std::any::Any;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use crate::channel::{ApplicativeHandler, NoCallBackApplicativeHandler, ProcessApplier, PublishHandler};
use crate::errors::AgentError;
use crate::{Applicator, Process, Publisher};
use self::any::DynRef;

pub mod any;

#[derive(Debug)]
pub struct Ref<T: Process> {
    pub(crate) channel: UnboundedSender<Box<dyn ProcessApplier<T>>>
}


impl<T: Process> Ref<T> {
    #[rustfmt::skip]
    pub async fn publish<C: Command>(&self, command: C) -> Result<Result<T::Event, T::Rejection>, AgentError>
    where
        T: Publisher<C>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(PublishHandler {
                command,
                oneshot: tx,
            }))
            .map_err(|_| AgentError::ChannelDropped)?;

        rx.await.map_err(|_| AgentError::ChannelDropped)
    }

    pub async fn apply<E: Event>(&self, event: E) -> Result<(), AgentError>
    where
        T: Applicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(ApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| AgentError::ChannelDropped)?;

        rx.await.map_err(|_| AgentError::ChannelDropped)
    }
    
    pub fn notify<E: Event>(&self, event: E) -> Result<(), AgentError>
    where 
        T: Applicator<E>
    {
        self.channel
            .send(Box::new(NoCallBackApplicativeHandler { event }))
            .map_err(|_| AgentError::ChannelDropped)
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