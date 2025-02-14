use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;

use async_trait::async_trait;
use futures_util::FutureExt;
use nitinol_core::event::Event;

use crate::errors::ProjectionError;
use crate::projection::Projection;
use crate::resolver::ResolveMapping;

#[rustfmt::skip]
#[async_trait]
pub trait PatchHandler<T: ResolveMapping>: 'static + Sync + Send {
    async fn apply(&self, entity: &mut Option<T>, payload: Vec<u8>, seq: &mut i64) -> Result<(), ProjectionError>;
}

pub struct ProjectionResolver<T: ResolveMapping, E: Event> {
    _projection: PhantomData<T>,
    _event: PhantomData<E>,
}

impl<T: ResolveMapping, E: Event> Default for ProjectionResolver<T, E> {
    fn default() -> Self {
        Self {
            _projection: Default::default(),
            _event: Default::default(),
        }
    }
}

#[rustfmt::skip]
#[async_trait]
impl<T: ResolveMapping, E: Event> PatchHandler<T> for ProjectionResolver<T, E>
where
    T: Projection<E>,
{
    async fn apply(&self, entity: &mut Option<T>, payload: Vec<u8>, seq: &mut i64) -> Result<(), ProjectionError> {
        *seq += 1;
        let event = E::from_bytes(&payload)?;
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
                },
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
