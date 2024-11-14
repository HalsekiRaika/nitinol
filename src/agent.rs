pub mod any;
mod applicator;
mod context;
pub mod errors;
pub mod handler;
pub mod lifecycle;
mod publisher;
mod registry;

pub use self::applicator::*;
pub use self::publisher::*;
pub use self::context::*;
pub use self::handler::*;
pub use self::registry::*;

use crate::agent::any::DynAgent;
use crate::agent::errors::AgentError;
use crate::mapping::ResolveMapping;
use crate::Event;
use std::any::Any;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

pub trait Command: 'static + Sync + Send {}

#[derive(Debug)]
pub struct Agent<T: ResolveMapping> {
    sender: UnboundedSender<Box<dyn Applier<T>>>,
}

impl<T: ResolveMapping> Clone for Agent<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<T: ResolveMapping> Agent<T> {
    pub(crate) fn new(sender: UnboundedSender<Box<dyn Applier<T>>>) -> Self {
        Self { sender }
    }
}

impl<T: ResolveMapping> DynAgent for Agent<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<T: ResolveMapping> Agent<T> {
    pub async fn publish<C: Command>(
        &self,
        command: C,
    ) -> Result<Result<T::Event, T::Rejection>, AgentError>
    where
        T: Publisher<C>,
    {
        let (tx, rx) = oneshot::channel();
        self.sender
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
        self.sender
            .send(Box::new(ApplicativeHandler { event, oneshot: tx }))
            .map_err(|_| AgentError::ChannelDropped)?;

        rx.await.map_err(|_| AgentError::ChannelDropped)
    }
}

#[async_trait::async_trait]
pub(crate) trait Applier<T: ResolveMapping>: 'static + Sync + Send {
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), AgentError>;
}
