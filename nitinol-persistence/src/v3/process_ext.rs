use nitinol_core::event::Event;
use nitinol_process::{Context, Process};
use async_trait::async_trait;
use nitinol_core::identifier::ToEntityId;
use crate::v3::error::PersistErr;
use crate::v3::process::JournalProcess;

#[async_trait]
pub trait PersistentProcess: Process {
    async fn persist<E: Event>(event: E, ctx: &mut Context) -> Result<E, PersistErr> {
        if let Some(refs) = ctx.find::<JournalProcess>(&"journal-process".to_entity_id()).await {
        }
        Ok(event)
    }
}
