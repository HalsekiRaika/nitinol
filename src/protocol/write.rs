use std::sync::Arc;
use async_trait::async_trait;
use crate::Event;
use crate::identifier::EntityId;
use crate::protocol::errors::ProtocolError;

#[async_trait]
pub trait WriteProtocol: 'static + Sync + Send {
    async fn write(&self, id: &EntityId, seq: i64, payload: Vec<u8>) -> Result<(), ProtocolError>;
}

pub struct Writer {
    writer: Arc<dyn WriteProtocol>,
}

impl Clone for Writer {
    fn clone(&self) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
        }
    }
}

impl Writer {
    pub fn new(provider: impl WriteProtocol) -> Self {
        Self {
            writer: Arc::new(provider),
        }
    }

    pub async fn write<E: Event>(&self, id: &EntityId, seq: i64, payload: &E) -> Result<(), ProtocolError> {
        let payload = payload.as_bytes()
            .map_err(|e| ProtocolError::Write(Box::new(e)))?;
        self.writer.write(id, seq, payload).await
    }
}