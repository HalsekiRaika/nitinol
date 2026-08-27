use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nitinol_contract::Event;
use nitinol_persistence::EventType;

use crate::codec::ErasedCodec;
use crate::projection::context::ProjectionContext;
use crate::projection::projector::Projector;

/// Type-erased event dispatch entry used during catch-up.
///
/// Each registered event type produces one `EventTypeHandler`.  The handler
/// decodes raw bytes from the event store and delegates to the corresponding
/// `Projector<E, Tx>::project` implementation.
#[async_trait]
pub(crate) trait EventTypeHandler<P: Send + Sync, Tx: Send>: Send + Sync {
    fn event_type(&self) -> EventType;

    async fn handle(
        &self,
        projector: &mut P,
        payload: Bytes,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub(crate) struct ConcreteHandler<P, E> {
    pub codec: Arc<dyn ErasedCodec<E>>,
    pub _phantom: PhantomData<P>,
}

#[async_trait]
impl<P, E, Tx> EventTypeHandler<P, Tx> for ConcreteHandler<P, E>
where
    P: Projector<E, Tx> + Send + Sync + 'static,
    E: Event,
    Tx: Send + 'static,
{
    fn event_type(&self) -> EventType {
        E::EVENT_TYPE
    }

    async fn handle(
        &self,
        projector: &mut P,
        payload: Bytes,
        ctx: &mut ProjectionContext<'_, Tx>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let event = self
            .codec
            .decode(&payload)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        projector
            .project(event, ctx)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}
