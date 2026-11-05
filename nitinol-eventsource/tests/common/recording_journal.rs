//! A journal that records when each stream was asked for.
//!
//! A pull consumer's cadence is only visible from the store's side: what a
//! poller does between two reads leaves nothing behind, so the moment of each
//! `load` is what a clause about "how long it waited" can read.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

const RECORD_LOCK: &str = "the record lock guards only a push or a copy, so no holder panics";

/// How often a wait below looks at the record again.
const PROBE_INTERVAL: Duration = Duration::from_millis(10);

/// One `load`: the stream it named, if any, and when it was asked.
type Load = (Option<String>, Instant);

/// An [`EventStore`] that answers from memory and notes, for every `load`, the
/// stream it named and when it was asked.
///
/// Its `Clone` is the handle's: the record is one region behind every handle,
/// so the store named in a wiring and the handle a clause reads see the same
/// calls.
#[derive(Clone, Default)]
pub struct RecordingJournal {
    inner: InMemoryEventStore,
    loads: Arc<Mutex<Vec<Load>>>,
}

impl RecordingJournal {
    /// When `stream_key` was asked for, in call order.
    ///
    /// Only loads that name the key count: a projector following one aggregate
    /// names its stream on every read, and anything else sharing the journal
    /// names something else.
    pub fn loads_of(&self, stream_key: &str) -> Vec<Instant> {
        self.loads
            .lock()
            .expect(RECORD_LOCK)
            .iter()
            .filter(|(key, _)| key.as_deref() == Some(stream_key))
            .map(|(_, at)| *at)
            .collect()
    }

    /// The gaps between consecutive loads of `stream_key`, in call order.
    pub fn gaps_of(&self, stream_key: &str) -> Vec<Duration> {
        self.loads_of(stream_key)
            .windows(2)
            .map(|pair| pair[1].saturating_duration_since(pair[0]))
            .collect()
    }

    /// Wait until `stream_key` has been asked for at least `want` times, and
    /// answer when each of those loads came.
    ///
    /// Panics once `within` has passed without that many loads, naming how many
    /// there were.
    pub async fn wait_until_loads_of(
        &self,
        stream_key: &str,
        want: usize,
        within: Duration,
    ) -> Vec<Instant> {
        let deadline = Instant::now() + within;
        loop {
            let loads = self.loads_of(stream_key);
            if loads.len() >= want {
                return loads;
            }
            assert!(
                Instant::now() < deadline,
                "`{stream_key}` was asked for {} times in {within:?}, wanted {want}",
                loads.len()
            );
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }
}

#[async_trait]
impl EventStore for RecordingJournal {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.loads
            .lock()
            .expect(RECORD_LOCK)
            .push((query.stream_key.clone(), Instant::now()));
        self.inner.load(query).await
    }
}
