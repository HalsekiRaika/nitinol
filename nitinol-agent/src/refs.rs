use std::any::Any;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;
use nitinol_core::resolver::ResolveMapping;
use crate::channel::{ApplicativeHandler, Applier, PublishHandler};
use crate::errors::AgentError;
use crate::{Applicator, Publisher};
use self::any::DynRef;

pub mod any;

#[derive(Debug)]
pub struct Ref<T: ResolveMapping> {
    pub(crate) channel: UnboundedSender<Box<dyn Applier<T>>>
}


impl<T: ResolveMapping> Ref<T> {
    pub async fn publish<C: Command>(
        &self,
        command: C,
    ) -> Result<Result<T::Event, T::Rejection>, AgentError>
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
}

impl<T: ResolveMapping> Clone for Ref<T> {
    fn clone(&self) -> Self {
        Self { 
            channel: self.channel.clone()
        }
    }
}

impl<T: ResolveMapping> DynRef for Ref<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}