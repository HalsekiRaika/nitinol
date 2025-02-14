use std::fmt::Debug;
use async_trait::async_trait;
use nitinol_core::event::Event;

#[async_trait]
pub trait Projection<E: Event>: 'static + Sync + Send + Sized {
    type Rejection: Debug + 'static + Sync + Send;
    #[allow(unused_variables)]
    async fn first(event: E) -> Result<Self, Self::Rejection> {
        unimplemented!("starting point process is not implemented.");
    }
    async fn apply(&mut self, event: E) -> Result<(), Self::Rejection>;
}
