//! Read model and projector for the projection example.
//!
//! `CountReadModel` is an in-memory read model maintained by `CountProjector`.
//! The projector implements `Projector<Incremented>` and updates shared state
//! on each event.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Notify;

use nitinol_eventsource::{ProjectionContext, Projector};

use crate::counter::Incremented;

/// Shared read model storing the current count.
#[derive(Default)]
pub struct CountReadModel {
    pub value: Arc<AtomicU64>,
    /// Notified each time the read model is updated.
    pub updated: Arc<Notify>,
}

/// Projector that maintains `CountReadModel` by counting `Incremented` events.
pub struct CountProjector {
    pub model: CountReadModel,
}

#[async_trait]
impl Projector<Incremented> for CountProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.model.value.fetch_add(1, Ordering::SeqCst);
        self.model.updated.notify_one();
        Ok(())
    }
}
