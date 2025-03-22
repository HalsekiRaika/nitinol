use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_process::{Context, Process};

#[async_trait]
pub trait WithPersistence: 'static + Sync + Send
where
    Self: Process,
{
    async fn persist<E: Event>(&self, event: &E, ctx: &mut Context) {
        crate::global::get_global_writer()
            .write(self.aggregate_id(), event, ctx.sequence())
            .await;
    }
}

impl<T> WithPersistence for T where T: Process {}