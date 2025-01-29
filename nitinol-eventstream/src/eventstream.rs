use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::{self, Sender as BroadcastSender, Receiver};
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_process::Ref;
use nitinol_protocol::Payload;
use crate::extension::WithEventSubscriber;
use crate::resolver::{DecodeMapping, DecodeResolver, SubscriptionMapper};

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

    pub async fn subscribe<S: SubscriptionMapper>(&self, subscribe: S) {
        let mut mapping = DecodeMapping::default();
        S::mapping(&mut mapping);
        
        let subscribe_rx = self.root.subscribe();

        tokio::spawn(async move {
            let mapping = mapping;
            let mut subscriber = subscribe;
            let mut subscribe_rx = subscribe_rx;

            loop {
                match subscribe_rx.recv().await {
                    Ok(payload) => {
                        if let Some((_, handler)) = mapping.map.iter().find(|(id, _)| id.eq(&&payload.registry_key)) {
                            handler.apply(&mut subscriber, &payload.bytes).await;
                        } else {
                            tracing::warn!("No handler found for event: {}#{}", payload.id, payload.registry_key);
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
            }
        });
    }
    
    pub(crate) async fn subscribe_to_process<E: Event, P: WithEventSubscriber<E>>(&self, refs: Ref<P>) {
        let resolver = DecodeResolver::new(refs);
        let subscribe_rx = self.root.subscribe();
        tokio::spawn(async move {
            let mut subscribe_rx = subscribe_rx;

            loop {
                match subscribe_rx.recv().await {
                    Ok(payload) => {
                        if payload.registry_key.eq(E::REGISTRY_KEY) {
                            resolver.apply(&payload.bytes).await;
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
            }
        });
    }
}
