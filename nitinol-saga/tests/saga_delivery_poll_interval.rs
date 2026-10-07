//! How often the host asks is set where the host is wired.
//!
//! ```gherkin
//!   Scenario: a deployment chooses how often to poll
//!     Given a delivery host wired with a poll interval
//!     When it runs for a while with nothing to deliver
//!     Then the gap between its queries is at least that interval
//!     And a host wired with a shorter interval queries more often in the
//!         same window
//! ```
//!
//! The two halves are both needed. The lower bound alone would pass a host
//! that ignored the setting and polled more slowly than asked for some other
//! reason; the contrast alone would pass one that treated the setting as a
//! vague hint. Together they say the value configured is the value used.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    delivery_id, delivery_persistence, in_memory_system, partition_count, spawn_host, DeliverySaga,
    RecordingCheckpointStore, RecordingEventStore, TEST_POLL_INTERVAL,
};

use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_saga::SagaDeliveryProps;

/// Deliberately far from any framework default, and far enough above it that a
/// host polling at a default would be caught by the count below.
const SLOW: Duration = Duration::from_millis(500);

/// Long enough for several slow polls, so the count is about the interval
/// rather than about startup.
const WINDOW: Duration = Duration::from_millis(1_800);

#[tokio::test]
async fn the_configured_interval_is_the_interval_between_queries() {
    // Given: nothing to deliver, so the gaps are the poll interval and not the
    // time taken by work.
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let checkpoints = RecordingCheckpointStore::new();

    // When
    let _host = SagaDeliveryProps::<DeliverySaga, Arc<RecordingCheckpointStore>>::new(
        &delivery_persistence(&journal, &checkpoints),
    )
    .with_delivery_id(delivery_id())
    .with_partitions(partition_count(1))
    .with_poll_interval(SLOW)
    .spawn(&system)
    .await;
    tokio::time::sleep(WINDOW).await;

    // Then
    let gaps = recording.load_gaps();
    assert!(
        gaps.len() >= 2,
        "the host must poll more than once in {WINDOW:?}; it polled {} times",
        recording.load_count()
    );
    for (nth, gap) in gaps.iter().enumerate() {
        assert!(
            *gap >= SLOW - Duration::from_millis(50),
            "gap {nth} was {gap:?}, which is shorter than the configured \
             {SLOW:?}: the interval the wiring named is not being used"
        );
    }
}

#[tokio::test]
async fn a_shorter_interval_polls_more_often_in_the_same_window() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let checkpoints = RecordingCheckpointStore::new();

    // When
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &checkpoints, 1).await;
    tokio::time::sleep(WINDOW).await;

    // Then
    let slow_polls_in_window = WINDOW.as_millis() / SLOW.as_millis();
    assert!(
        u128::try_from(recording.load_count()).unwrap_or(u128::MAX) > slow_polls_in_window * 3,
        "a host wired with {TEST_POLL_INTERVAL:?} polled {} times in \
         {WINDOW:?}, which is not meaningfully more than the {slow_polls_in_window} \
         a host wired with {SLOW:?} manages — the setting is being ignored",
        recording.load_count()
    );
}
