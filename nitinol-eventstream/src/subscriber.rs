use std::fmt::Debug;

use async_trait::async_trait;
use nitinol_core::event::Event;

#[async_trait]
pub trait EventSubscriber<E: Event>: 'static + Sync + Send {
    type Error: Debug + Sync + Send + 'static;
    async fn on(&mut self, event: E) -> Result<(), Self::Error>;
}
