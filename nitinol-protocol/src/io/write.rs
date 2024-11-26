use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_core::identifier::{EntityId, ToEntityId};
use crate::errors::ProtocolError;
use crate::Payload;

#[async_trait]
pub trait Writer: 'static + Sync + Send {
    async fn write(&self, aggregate_id: EntityId, payload: Payload) -> Result<(), ProtocolError>;
}

pub struct WriteProtocol {
    writer: Arc<dyn Writer>
}

impl Debug for WriteProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteProtocol").finish()
    }
}

impl Clone for WriteProtocol {
    fn clone(&self) -> Self {
        Self { writer: Arc::clone(&self.writer) }
    }
}

impl WriteProtocol {
    pub fn new(provider: impl Writer) -> Self {
        Self { writer: Arc::new(provider) }
    }
    
    pub async fn write<E: Event>(&self, aggregate_id: impl ToEntityId, event: &E, seq: i64) -> Result<(), ProtocolError> {
        let event = event.as_bytes().map_err(|e| ProtocolError::Write(Box::new(e)))?;
        self.writer
            .write(aggregate_id.to_entity_id(), Payload {
                sequence_id: seq,
                registry_key: E::REGISTRY_KEY.to_string(),
                bytes: event,
            })
            .await
    }
}
