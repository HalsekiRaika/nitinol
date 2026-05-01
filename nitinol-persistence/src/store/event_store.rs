use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::{AppendError, LoadError};
use crate::event::{AppendingEvent, LoadedEvent};
use crate::id::AggregateId;
use crate::query::{AppendOutcome, LoadQuery};

pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'a>>;

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        aggregate_id: &AggregateId,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError>;

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError>;
}
