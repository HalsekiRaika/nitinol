use crate::extension::PersistenceExtension;
use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_core::identifier::ToEntityId;
use nitinol_process::{Context, FromContextExt, Process};

#[async_trait]
pub trait WithPersistence: 'static + Sync + Send
where
    Self: Process,
{
    fn aggregate_id(&self) -> impl ToEntityId;
    async fn persist<E: Event>(&self, event: &E, ctx: &mut Context) {
        let ext = match PersistenceExtension::from_context(ctx) {
            Ok(ext) => ext,
            Err(e) => panic!("{}", e),
        };

        ext.persist(self.aggregate_id(), event, ctx.sequence() + 1)
            .await;
    }
}
