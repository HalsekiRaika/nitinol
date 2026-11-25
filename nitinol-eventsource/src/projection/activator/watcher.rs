//! The activator's watcher: a slow read of the whole journal, for appends no
//! notification announced.
//!
//! It reads from the journal's head as the first projector is spawned and
//! keeps no position: what other processes wrote before that is read by each
//! projector's own catch-up when it is first activated. Should the head not be
//! readable, it reads from the journal's start instead, so that nothing
//! written between a projector's catch-up and the watcher's first read is
//! passed over.

use std::time::Duration;

use nitinol_persistence::store::Journal;
use nitinol_persistence::{EventType, LoadedEvent};

/// How long the watcher waits between reads unless the system names an
/// interval: forty times a projector's default poll.
///
/// It only finds what was appended outside the system's own journal, which is
/// another process's writes, and a projection already running reads those at
/// its own poll; the watcher is what wakes a passivated one.
const DEFAULT_WATCHER_INTERVAL: Duration = Duration::from_secs(10);

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

/// What the watcher reads, and how often.
#[derive(Clone)]
pub(crate) struct WatcherSource {
    pub(crate) journal: Journal,
    pub(crate) interval: Duration,
}

impl WatcherSource {
    /// A watcher over `journal`, every `interval`.
    pub(crate) fn new(journal: Journal, interval: Duration) -> Self {
        Self { journal, interval }
    }
}

/// What the watcher keeps of a record it read: where it was appended, and of
/// which type. The payload is dropped as it is read.
pub(crate) struct Seen {
    pub(crate) stream_key: String,
    pub(crate) event_type: EventType,
    pub(crate) global_sequence: u64,
    /// Its sequence in its own stream.
    pub(crate) sequence: u64,
}

impl From<LoadedEvent> for Seen {
    fn from(loaded: LoadedEvent) -> Self {
        Self {
            stream_key: loaded.stream_key,
            event_type: loaded.event_type,
            global_sequence: loaded.global_sequence,
            sequence: loaded.sequence,
        }
    }
}
