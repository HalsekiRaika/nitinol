use crate::extension::PersistenceExtension;
use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_process::{Context, FromContextExt, Process};

#[async_trait]
pub trait WithPersistence: 'static + Sync + Send
where
    Self: Process,
{
    fn aggregate_id(&self) -> EntityId;
    async fn persist<E: Event>(&self, event: &E, ctx: &mut Context) {
        let Ok(ext) = PersistenceExtension::from_context(ctx) else {
            panic!("Persistence extension is not found.");
        };

        ext.persist(self.aggregate_id(), event, ctx.sequence())
            .await;
    }
}
