use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_process::{Context, Process};

#[async_trait]
pub trait WithStreamPublisher: 'static + Sync + Send 
where
    Self: Process
{
    async fn publish<E: Event>(&self, event: &E, ctx: &mut Context) {
        crate::global::get_event_stream()
            .publish(self.aggregate_id(), ctx.sequence(), event)
            .await;
    }
}

impl<T> WithStreamPublisher for T where T: Process {}