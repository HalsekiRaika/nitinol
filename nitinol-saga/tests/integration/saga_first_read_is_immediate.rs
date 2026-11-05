//! A saga's pollers read once as soon as they start, and wait their interval
//! only between reads.
//!
//! ```gherkin
//!   Scenario: the delivery host's first read does not wait for its interval
//!     Given a delivery host wired with an interval far longer than the default
//!     When the saga is spawned
//!     Then the host has asked for its saga's decisions well inside that
//!          interval
//!
//!   Scenario: an observed source's first read does not wait for its interval
//!     Given a source bound with an interval far longer than the default
//!     When the saga is spawned
//!     Then the source has asked for its stream well inside that interval
//! ```
//!
//! Each half of a saga drives its reads with a poller of its own, so each is
//! observed on its own: the delivery host's loads are the ones naming the
//! saga's decision type, a source's are the ones naming the stream it is bound
//! to. The other half is left at its own cadence and is not what a clause
//! reads.
//!
//! That the reads after the first keep their interval is already what
//! `saga_delivery_poll_interval.rs` and `saga_subscription_poll_interval.rs`
//! pin, so it is not restated here.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test integration saga_first_read_is_immediate::`.

use super::delivery;
use delivery::{
    delivery_id, delivery_system, partition_count, wait_until_loads, DeliverySaga,
    RecordingEventStore, RecordingOffsetStore, Triggered, FIXTURE_LOCK, PROBE_INTERVAL,
    PROBE_TIMEOUT, TRIGGER_KEY,
};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};
use nitinol_saga::{SagaManagerProps, Subscription};

/// Far longer than the default, so a first read that waited for it cannot pass
/// for one that did not.
const LONG: Duration = Duration::from_secs(2);

/// Far shorter than [`LONG`], and still loose enough for a loaded machine.
const WELL_INSIDE: Duration = Duration::from_millis(500);

/// The stream the observed-source scenario binds its source to.
const UPSTREAM_KEY: &str = "saga-first-read-is-immediate-upstream";

/// An `EventStore` that records when [`UPSTREAM_KEY`] was asked for.
///
/// The delivery host started by the same spawn reads this journal too, and
/// names its saga's decision type rather than a stream key, so counting only
/// the calls naming the key leaves the source's reads.
#[derive(Clone, Default)]
struct UpstreamRecorder {
    inner: InMemoryEventStore,
    reads: Arc<Mutex<Vec<Instant>>>,
}

impl UpstreamRecorder {
    fn read_count(&self) -> usize {
        self.reads.lock().expect(FIXTURE_LOCK).len()
    }

    /// Wait until the source has asked for its stream at least once.
    async fn wait_for_a_read(&self) {
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while self.read_count() == 0 {
            assert!(
                Instant::now() < deadline,
                "the source never asked for `{UPSTREAM_KEY}` in {PROBE_TIMEOUT:?}"
            );
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }
}

#[async_trait]
impl EventStore for UpstreamRecorder {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if query.stream_key.as_deref() == Some(UPSTREAM_KEY) {
            self.reads.lock().expect(FIXTURE_LOCK).push(Instant::now());
        }
        self.inner.load(query).await
    }
}

#[tokio::test]
async fn the_delivery_host_reads_once_before_its_interval_has_passed() {
    // Given
    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&recording, &offsets).await;

    // When
    let started = Instant::now();
    let _host = SagaManagerProps::<DeliverySaga>::new(|_| DeliverySaga)
        .observing::<Triggered>(Subscription::stream(TRIGGER_KEY))
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(1))
        .with_delivery_poll_interval(LONG)
        .spawn(&system)
        .await;
    wait_until_loads(&recording, 1, "the delivery host must read its journal").await;
    let first_read_in = started.elapsed();

    // Then
    assert!(
        first_read_in < WELL_INSIDE,
        "a delivery host polling every {LONG:?} first asked for its decisions \
         {first_read_in:?} after the spawn; its first read waited for the \
         interval"
    );
}

#[tokio::test]
async fn an_observed_source_reads_once_before_its_interval_has_passed() {
    // Given
    let recorder = UpstreamRecorder::default();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&recorder, &offsets).await;

    // When
    let started = Instant::now();
    let _host = SagaManagerProps::<DeliverySaga>::new(|_| DeliverySaga)
        .observing::<Triggered>(Subscription::stream(UPSTREAM_KEY).with_poll_interval(LONG))
        .spawn(&system)
        .await;
    recorder.wait_for_a_read().await;
    let first_read_in = started.elapsed();

    // Then
    assert!(
        first_read_in < WELL_INSIDE,
        "a source bound with {LONG:?} first asked for `{UPSTREAM_KEY}` \
         {first_read_in:?} after the spawn; its first read waited for the \
         interval"
    );
}
