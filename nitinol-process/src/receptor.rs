use std::any::Any;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use nitinol_core::command::Command;
use nitinol_core::event::Event;

use crate::channel::{ProcessApplier, CommandReceptor, ApplicativeReceptor, TryApplicativeReceptor, NonBlockingEntrustHandler};
use crate::errors::ChannelDropped;
use crate::{EventApplicator, Process, CommandHandler, TryEventApplicator};

pub mod any;

use self::any::DynRef;

#[derive(Debug)]
pub struct Receptor<T: Process> {
    pub(crate) channel: UnboundedSender<Box<dyn ProcessApplier<T>>>
}

#[rustfmt::skip]
impl<T: Process> Receptor<T> {
    pub async fn handle<C: Command>(&self, command: C) -> Result<Result<T::Event, T::Rejection>, ChannelDropped>
    where
        T: CommandHandler<C>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(CommandReceptor {
                command,
                oneshot: tx,
            }))
            .map_err(|_| ChannelDropped)?;

        rx.await.map_err(|_| ChannelDropped)
    }

    pub async fn apply<E: Event>(&self, event: E) -> Result<(), ChannelDropped>
    where
        T: EventApplicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(ApplicativeReceptor { event, oneshot: tx }))
            .map_err(|_| ChannelDropped)?;

        rx.await.map_err(|_| ChannelDropped)
    }
    
    pub async fn try_apply<E: Event>(&self, event: E) -> Result<Result<(), T::Rejection>, ChannelDropped>
    where
        T: TryEventApplicator<E>,
    {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(Box::new(TryApplicativeReceptor { event, oneshot: tx }))
            .map_err(|_| ChannelDropped)?;
        
        rx.await.map_err(|_| ChannelDropped)
    }
    
    pub async fn entrust<C: Command>(&self, cmd: C) -> Result<(), ChannelDropped>
    where
        T: CommandHandler<C>,
        T: EventApplicator<<T as CommandHandler<C>>::Event>,
    {
        self.channel
            .send(Box::new(NonBlockingEntrustHandler { command: cmd }))
            .map_err(|_| ChannelDropped)?;
        
        Ok(())
    }
}

impl<T: Process> Clone for Receptor<T> {
    fn clone(&self) -> Self {
        Self { 
            channel: self.channel.clone()
        }
    }
}

impl<T: Process> DynRef for Receptor<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}