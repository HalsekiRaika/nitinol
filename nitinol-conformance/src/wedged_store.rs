use std::sync::Arc;

use async_trait::async_trait;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

/// A store that answers every read and records nothing.
///
/// A clause that needs "the machinery got in the way" needs the interpreter to
/// have replayed its history and reached a decision first, so loading has to
/// keep working while appending does not.
pub(crate) struct WedgedStore {
    backing: Arc<dyn EventStore>,
}

impl WedgedStore {
    pub(crate) fn over(backing: Arc<dyn EventStore>) -> Self {
        Self { backing }
    }
}

/// What the wedge reports as the backend's failure.
///
/// A named type rather than a bare message, so that an interpreter that
/// forwards it is forwarding a failure of the store and not a value the suite
/// could be mistaken for a domain refusal.
#[derive(Debug, thiserror::Error)]
#[error("this store is wedged: it answers reads and records nothing")]
struct Wedged;

#[async_trait]
impl EventStore for WedgedStore {
    async fn append(
        &self,
        _key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        Err(AppendError::Backend(Box::new(Wedged)))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.backing.load(query).await
    }
}
