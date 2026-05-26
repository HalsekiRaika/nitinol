use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::{AppendError, LoadError};
use crate::event::{AppendingEvent, LoadedEvent};
use crate::query::{AppendOutcome, LoadQuery};

pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'a>>;

/// Append-and-load surface for events.
///
/// The `append` and `load` methods take the stream key as `&str` rather than a
/// typed identifier so the trait remains dyn-compatible (`Arc<dyn EventStore>`
/// is the framework's primary handle for plugging in a backend).  Callers
/// supply the key via `id.borrow()` (or `id.as_str()`) on their newtype —
/// `AggregateId` and `SagaId` both implement `Borrow<str>`.
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError>;

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError>;
}
