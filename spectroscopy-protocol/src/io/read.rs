use std::collections::BTreeSet;
use std::fmt::Debug;
use std::sync::Arc;
use async_trait::async_trait;
use spectroscopy_core::event::Event;
use crate::errors::ProtocolError;
use crate::Payload;

#[async_trait]
pub trait Reader: 'static + Sync + Send {
    async fn read(&self, id: &str, seq: i64) -> Result<Payload, ProtocolError>;
    async fn read_to(&self, id: &str, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError>;
    async fn read_to_latest(&self, id: &str, from: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.read_to(id, from, i64::MAX).await
    }
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
    
    async fn read<E: Event>(&self, id: &str, seq: i64) -> Result<E, ProtocolError> {
        let payload = self.reader.read(id, seq).await?;
        E::from_bytes(&payload.bytes)
            .map_err(|e| ProtocolError::Read(Box::new(e)))
    }
    
    async fn read_to(&self, id: &str, from: i64, to: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_to(id, from, to).await
    }
    
    async fn read_to_latest(&self, id: &str, from: i64) -> Result<BTreeSet<Payload>, ProtocolError> {
        self.reader.read_to_latest(id, from).await
    }
}