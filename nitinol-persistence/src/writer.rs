use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_protocol::io::{WriteProtocol, Writer};

#[derive(Debug, Clone)]
pub struct EventWriter {
    writer: WriteProtocol,
    retry: i64
}

impl EventWriter {
    pub fn new(writer: impl Writer) -> EventWriter {
        Self { writer: WriteProtocol::new(writer), retry: 3 }
    }
    
    pub fn set_retry(mut self, retry: i64) -> Self {
        self.retry = retry;
        self
    }
}

impl EventWriter {
    pub(crate) async fn write<E: Event>(&self, id: EntityId, event: &E, seq: i64) {
        let mut retry = 0;
        loop {
            match self.writer.write(id.clone(), event, seq).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::error!("on failure persist caused reason `{e}`");
                    
                    retry += 1;
                    
                    if retry >= self.retry {
                        tracing::error!("retry limit exceeded");
                        break;
                    } else {
                        continue;
                    }
                }
            }
        }
    }
}