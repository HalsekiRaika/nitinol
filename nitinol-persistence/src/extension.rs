use nitinol_core::event::Event;
use nitinol_core::identifier::{EntityId, ToEntityId};
use nitinol_process::FromContextExt;
use nitinol_protocol::io::{WriteProtocol, Writer};

#[derive(Debug, Clone)]
pub struct PersistenceExtension {
    ext: WriteProtocol,
}

impl FromContextExt for PersistenceExtension {}

impl PersistenceExtension {
    pub fn new(writer: impl Writer) -> PersistenceExtension {
        Self {
            ext: WriteProtocol::new(writer),
        }
    }
}

impl PersistenceExtension {
    pub(crate) async fn persist<E: Event>(&self, id: EntityId, event: &E, seq: i64) {
        loop {
            match self.ext.write(id.clone(), event, seq).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::error!("on failure persist caused reason `{e}`");
                    continue;
                }
            }
        }
    }
}
