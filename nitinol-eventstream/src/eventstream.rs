use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Sender as BroadcastSender;
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_protocol::Payload;

use crate::resolver::{DecodeMapping, SubscriptionMapper};

#[derive(Clone)]
pub struct EventStream {
    root: BroadcastSender<Payload>,
}

impl Default for EventStream {
    fn default() -> Self {
        Self {
            root: BroadcastSender::new(256),
        }
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
}
