use std::collections::BTreeSet;
use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_core::identifier::{EntityId, ToEntityId};
use crate::errors::ProtocolError;
use crate::Payload;

#[async_trait]
pub trait Reader: 'static + Sync + Send {
    async fn read(&self, id: EntityId, seq: i64) -> Result<Payload, ProtocolError>;
    async fn read_to(&self, id: EntityId, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError>;
    async fn read_to_latest(&self, id: EntityId, from: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.read_to(id, from, i64::MAX).await
    }
    async fn read_all_by_registry_key(&self, key: &str) -> Result<BTreeSet<Payload>, ProtocolError>;
}


pub struct ReadProtocol {
    reader: Arc<dyn Reader>,
}

impl Debug for ReadProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadProtocol").finish()
    }
}

impl Clone for ReadProtocol {
    fn clone(&self) -> Self {
        Self {
            reader: Arc::clone(&self.reader),
        }
    }
}

impl ReadProtocol {
    pub fn new(provider: impl Reader) -> Self {
        Self {
            reader: Arc::new(provider),
        }
    }
    
    pub async fn read<E: Event>(&self, id: impl ToEntityId, seq: i64) -> Result<E, ProtocolError> {
        let payload = self.reader.read(id.to_entity_id(), seq).await?;
        E::from_bytes(&payload.bytes)
            .map_err(|e| ProtocolError::Read(Box::new(e)))
    }
    
    pub async fn read_to(&self, id: impl ToEntityId, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_to(id.to_entity_id(), from, to).await
    }
    
    pub async fn read_to_latest(&self, id: impl ToEntityId, from: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_to_latest(id.to_entity_id(), from).await
    }
    
    pub async fn read_all_by_event<E: Event>(&self) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_all_by_registry_key(E::REGISTRY_KEY).await
    }
    
    pub async fn read_all_by_key(&self, registry_key: &str) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_all_by_registry_key(registry_key).await
    }
}