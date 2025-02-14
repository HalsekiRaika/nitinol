use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;

use async_trait::async_trait;
use futures_util::FutureExt;
use nitinol_core::event::Event;

use crate::errors::{ProjectionError, UnimplementedError};
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

#[async_trait]
impl<T: ResolveMapping, E: Event> PatchHandler<T> for ProjectionResolver<T, E>
where
    T: Projection<E>,
{
    async fn apply(
        &self,
        entity: &mut Option<T>,
        payload: Vec<u8>,
        seq: &mut i64,
    ) -> Result<(), ProjectionError> {
        *seq += 1;
        let event = E::from_bytes(&payload)?;
        let Some(entity) = entity else {
            let a = AssertUnwindSafe(T::first(event))
                .catch_unwind()
                .await
                .map_err(|_| ProjectionError::Projection(Box::new(UnimplementedError)))?
                .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
            *entity = Some(a);
            return Ok(());
        };
        T::apply(entity, event)
            .await
            .map_err(|e| ProjectionError::Projection(Box::new(e)))?;
        
        Ok(())
    }
}
