use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

/// A store that delegates every call to `backing` and counts how many times it
/// was asked to append.
///
/// One append carrying every fact of a decision and several appends that
/// happen to land the same facts in the same order leave the same stream
/// behind; only the call count tells them apart, which is what L-2's atomicity
/// clause needs to tell a single atomic append from several that merely
/// succeeded in order.
pub(crate) struct CountingStore {
    backing: Arc<dyn EventStore>,
    appends: Arc<AtomicUsize>,
}

impl CountingStore {
    /// Wrap `backing`, returning the store and a handle to read its count.
    pub(crate) fn over(backing: Arc<dyn EventStore>) -> (Self, AppendCount) {
        let appends = Arc::new(AtomicUsize::new(0));
        (
            Self {
                backing,
                appends: Arc::clone(&appends),
            },
            AppendCount(appends),
        )
    }
}

/// How many times a [`CountingStore`] has been asked to append.
pub(crate) struct AppendCount(Arc<AtomicUsize>);

impl AppendCount {
    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EventStore for CountingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        self.backing.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.backing.load(query).await
    }
}
