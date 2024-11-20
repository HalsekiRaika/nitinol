use std::error::Error;
use crate::event::Event;

#[async_trait::async_trait]
pub trait Projection<E: Event>: 'static + Sync + Send + Sized {
    type Rejection: Error + 'static + Sync + Send;
    #[allow(unused_variables)]
    async fn first(event: E) -> Result<Self, Self::Rejection> {
        unimplemented!("starting point process is not implemented.");
    }
    async fn apply(&mut self, event: E) -> Result<(), Self::Rejection>;
}
