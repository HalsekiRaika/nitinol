use crate::extension::EventStreamExtension;
use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_process::{Context, FromContextExt, Process};

#[async_trait]
pub trait WithStreamPublisher: 'static + Sync + Send 
where
    Self: Process
{
    async fn publish<E: Event>(&self, event: &E, ctx: &mut Context) {
        let Ok(stream) = EventStreamExtension::from_context(ctx) else {
            panic!("`EventStreamExtension` not found in context");
        };
        
        stream.0.publish(self.aggregate_id(), ctx.sequence(), event).await;
    }
}
