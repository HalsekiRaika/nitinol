use async_trait::async_trait;
use nitinol_core::event::Event;
use nitinol_resolver::resolver::ResolveHandler;
use crate::subscriber::EventSubscriber;

pub(crate) const HANDLER_TYPE: &str = "subscribe";

pub struct Subscribe;

#[async_trait]
impl<E: Event, T> ResolveHandler<E, T> for Subscribe 
where
    T: EventSubscriber<E>,
{
    const HANDLER_TYPE: &'static str = HANDLER_TYPE;
    type Error = T::Error;
    
    async fn apply(entity: &mut Option<T>, event: E) -> Result<(), Self::Error> {
        let Some(entity) = entity else {
            panic!("Entity must exist in this process.");
        };
        
        entity.on(event).await?;
        Ok(())
    }
}
