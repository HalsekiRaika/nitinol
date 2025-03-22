use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::{self, Sender as BroadcastSender, Receiver};
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_process::Status;
use nitinol_protocol::Payload;
use nitinol_resolver::mapping::{Mapper, ResolveMapping};
use crate::process::WithEventSubscriber;

#[derive(Clone)]
pub struct EventStream {
    root: BroadcastSender<Payload>,
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStream {
    fn new() -> Self {
        let (root, terminal) = broadcast::channel(256);
       
        tokio::spawn(async move { 
            let mut dead_letter: Receiver<Payload> = terminal;
            while let Ok(payload) = dead_letter.recv().await { 
                tracing::trace!("Streamed event {}#{}", payload.id, payload.registry_key);
            }
        });
        
        Self { root }
    }
}

impl EventStream {
    pub async fn publish<E: Event>(&self, id: EntityId, seq: i64, event: &E) {
        self.root.send(Payload::new(id, seq, event).unwrap()).unwrap();
    }

    pub async fn subscribe<S: ResolveMapping>(&self, subscriber: S) {
        let mut mapping = Mapper::default();
        S::mapping(&mut mapping);
        
        let mapping = mapping.filter(|key| key.handler().eq(crate::resolver::HANDLER_TYPE));
        
        let rx = self.root.subscribe();

        tokio::spawn(async move {
            let mapping = mapping;
            let mut rx = rx;
            let mut subscriber = Some(subscriber);
            
            loop {
                match rx.recv().await {
                    Ok(payload) => {
                        if let Some(resolver) = mapping.find(|key| key.event().eq(&payload.registry_key)) {
                            if let Err(e) = resolver.resolve(&mut subscriber, &payload.bytes).await {
                                tracing::error!("{:?}", e);
                            }
                        }
                    }
                    Err(RecvError::Closed) => {
                        break;
                    }
                    Err(RecvError::Lagged(seq)) => {
                        tracing::warn!("Lagged event stream: {}", seq);
                    }
                }
            }
        });
    }
    
    pub(crate) async fn subscribe_in_process<E: Event, P: WithEventSubscriber<E>>(&self, mapping: Mapper<P>, status: Status) {
        let mapping = mapping.filter(|key| key.handler().eq(crate::process::resolver::RESOLVE_TYPE));
        let rx = self.root.subscribe();
        tokio::spawn(async move {
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(payload) => {
                        if let Some(resolver) = mapping.find(|key| key.event().eq(&payload.registry_key)) {
                            if let Err(e) = resolver.resolve(&mut None, &payload.bytes).await {
                                tracing::error!("{:?}", e);
                            }
                        }
                    }
                    Err(RecvError::Closed) => {
                        break;
                    }
                    Err(RecvError::Lagged(seq)) => {
                        tracing::warn!("Lagged event stream: {}", seq);
                        continue;
                    }
                }
                if !status.is_active().await {
                    break;
                }
            }
        });
    }
}
