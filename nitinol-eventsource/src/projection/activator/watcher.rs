//! The activator's watcher: a slow read of the whole journal, for appends no
//! notification announced.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{EventType, LoadedEvent};

/// How long the watcher waits between reads unless the system names an
/// interval: forty times a projector's default poll.
///
/// It only finds what was appended outside the system's own journal, which is
/// another process's writes, and a projection already running reads those at
/// its own poll; the watcher is what wakes a passivated one.
const DEFAULT_WATCHER_INTERVAL: Duration = Duration::from_secs(10);

/// The part of the framework the watcher's position is filed under.
const WATCHER_PART: &str = "projection";

/// Whether, and how often, a system's activator runs its watcher.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WatcherSetting {
    Every(Duration),
    Disabled,
}

impl Default for WatcherSetting {
    fn default() -> Self {
        Self::Every(DEFAULT_WATCHER_INTERVAL)
    }
}

/// What the watcher reads, where it keeps how far it has read, and how often.
#[derive(Clone)]
pub(crate) struct WatcherSource {
    pub(crate) journal: Journal,
    pub(crate) position: Arc<dyn WatcherPosition>,
    pub(crate) interval: Duration,
}

impl WatcherSource {
    /// A watcher over `journal`, every `interval`, keeping its position in
    /// `offsets` under the framework's reserved scope.
    pub(crate) fn new<Cs>(journal: Journal, offsets: Cs, interval: Duration) -> Self
    where
        Cs: OffsetStore + 'static,
    {
        Self {
            journal,
            position: Arc::new(FiledPosition {
                offsets,
                scope: OffsetScope::framework_owned(WATCHER_PART, "watcher", 0),
            }),
            interval,
        }
    }
}

/// How far the watcher has read, with the offset store's type erased: the node
/// that holds the activator names no store type.
#[async_trait]
pub(crate) trait WatcherPosition: Send + Sync {
    async fn load(&self) -> Result<Option<u64>, OffsetError>;

    async fn save(&self, position: u64) -> Result<(), OffsetError>;
}

struct FiledPosition<Cs> {
    offsets: Cs,
    scope: OffsetScope,
}

#[async_trait]
impl<Cs: OffsetStore> WatcherPosition for FiledPosition<Cs> {
    async fn load(&self) -> Result<Option<u64>, OffsetError> {
        self.offsets.load(&self.scope).await
    }

    async fn save(&self, position: u64) -> Result<(), OffsetError> {
        self.offsets.save(&self.scope, position, None).await
    }
}

/// What the watcher keeps of a record it read: where it was appended, and of
/// which type. The payload is dropped as it is read.
pub(crate) struct Seen {
    pub(crate) stream_key: String,
    pub(crate) event_type: EventType,
    pub(crate) global_sequence: u64,
}

impl From<LoadedEvent> for Seen {
    fn from(loaded: LoadedEvent) -> Self {
        Self {
            stream_key: loaded.stream_key,
            event_type: loaded.event_type,
            global_sequence: loaded.global_sequence,
        }
    }
}
