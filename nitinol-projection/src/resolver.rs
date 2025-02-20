use crate::errors::ProjectionError;
use crate::projection::Projection;
use async_trait::async_trait;
use futures_util::FutureExt;
use nitinol_core::event::Event;
use nitinol_resolver::resolver::ResolveHandler;
use std::panic::AssertUnwindSafe;

pub(crate) const HANDLER_TYPE: &str = "projection";

pub struct Project;

#[async_trait]
impl<E: Event, T> ResolveHandler<E, T> for Project
where
    T: Projection<E>,
{
    const HANDLER_TYPE: &'static str = HANDLER_TYPE;
    type Error = ProjectionError;

    async fn apply(entity: &mut Option<T>, event: E) -> Result<(), Self::Error> {
        let Some(entity) = entity else {
            let first = match AssertUnwindSafe(T::first(event))
                .catch_unwind()
                .await
                .map_err(|_| ProjectionError::FirstFormation)?
            {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!("First formation failed: {:?}", e);
                    return Err(ProjectionError::ApplyEvent {
                        backtrace: format!("{:?}", e),
                    });
                }
            };
            *entity = Some(first);
            return Ok(());
        };

        if let Err(e) = T::apply(entity, event).await {
            tracing::error!("Projection failed: {:?}", e);
            return Err(ProjectionError::ApplyEvent {
                backtrace: format!("{:?}", e),
            });
        }

        Ok(())
    }
}
