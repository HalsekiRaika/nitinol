use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_process::{Context, Process};

#[async_trait]
pub trait Persistence: 'static + Sync + Send 
where Self: Process
{
    async fn persist<E: Event>(&self, event: &E, ctx: &mut Context);
}