mod messages;

use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_core::identifier::{EntityId, ToEntityId};
use nitinol_process::task::{Receive};
use nitinol_process::{Context, Process};
use nitinol_protocol::io::WriteProtocol;
use crate::v3::error::PersistErr;
use crate::v3::process::messages::WriteEvent;

pub struct JournalProcess {
    protocol: WriteProtocol
}

impl Process for JournalProcess {
    fn aggregate_id(&self) -> EntityId {
        "journal-process".to_entity_id()
    }
}

#[async_trait]
impl<E: Event> Receive<WriteEvent<E>> for JournalProcess {
    type Error = PersistErr;
    
    async fn receive(&mut self, message: WriteEvent<E>, ctx: &mut Context) -> Result<(), Self::Error> {
        todo!()
    }
}