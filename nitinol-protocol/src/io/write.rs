use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use time::OffsetDateTime;
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
        let aggregate_id = aggregate_id.to_entity_id();
        self.writer
            .write(aggregate_id.clone(), Payload {
                id: aggregate_id.to_string(),
                sequence_id: seq,
                registry_key: E::REGISTRY_KEY.to_string(),
                bytes: event,
                created_at: OffsetDateTime::now_utc()
            })
            .await
    }
}
