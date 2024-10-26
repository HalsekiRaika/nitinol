use crate::event::Event;

#[async_trait::async_trait]
pub trait Projection<E: Event>: 'static + Sync + Send + Sized {
    type Rejection;
    async fn projection(this: &mut Option<Self>, event: E, seq: &mut u64) -> Result<(), Self::Rejection>;
}