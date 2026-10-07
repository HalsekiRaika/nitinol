//! A partition that is restarted resumes from what the store holds now.
//!
//! ```gherkin
//!   Scenario: a partition restarted under supervision
//!     Given a running delivery partition
//!     And a recorded position that has since moved on
//!     When the partition fails and is restarted
//!     Then its next query resumes above the recorded position as it now stands
//! ```
//!
//! The same partition lives across the restart — it is the host the wiring
//! started, supervised — so what it resumes from has to be read then rather
//! than settled once at startup. A position captured when the host was wired
//! would send a restarted partition back to where it began, re-delivering a
//! stretch an operator may have deliberately passed over.
//!
//! The journal is left empty on purpose: with nothing to deliver, the host
//! records no position of its own, so the only position in the store is the one
//! put there from outside and the query that follows the restart can only have
//! come from reading it.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    delivery_id, delivery_persistence, in_memory_system, partition_count, scope_of,
    wait_until_loads, DeliverySaga, LoadFault, RecordingCheckpointStore, RecordingEventStore,
};

use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_saga::SagaDeliveryProps;

/// Slow enough that the position below is in the store well before the query
/// that is meant to read it.
const POLL: Duration = Duration::from_millis(200);

/// The load call that fails, so the partition is restarted under supervision.
const FAILING_CALL: usize = 3;

/// A position no host in this test could have reached on its own.
const MOVED_ON_TO: u64 = 5;

#[tokio::test]
async fn a_restarted_partition_resumes_from_the_position_now_recorded() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let checkpoints = RecordingCheckpointStore::new();
    recording.fault_on(FAILING_CALL, LoadFault::Panic);

    let _host = SagaDeliveryProps::<DeliverySaga, Arc<RecordingCheckpointStore>>::new(
        &delivery_persistence(&journal, &checkpoints),
    )
    .with_delivery_id(delivery_id())
    .with_partitions(partition_count(1))
    .with_poll_interval(POLL)
    .spawn(&system)
    .await;

    wait_until_loads(&recording, 1, "the partition must start polling").await;

    // And: the recorded position moves on while the partition is running.
    checkpoints.put(&scope_of(0), MOVED_ON_TO);

    // When: the next-but-one load takes the partition down, and supervision
    // brings it back.
    let queries = wait_until_loads(
        &recording,
        FAILING_CALL + 2,
        "the partition must be restarted and poll again",
    )
    .await;

    // Then
    let before = &queries[..FAILING_CALL - 1];
    let after = &queries[FAILING_CALL..];

    assert!(
        before
            .iter()
            .all(|query| query.from_global_sequence == Some(1)),
        "the running partition kept its own position while it was alive: a \
         position written from outside is not a message to it; queries were \
         {before:?}"
    );
    assert!(
        after
            .iter()
            .all(|query| query.from_global_sequence == Some(MOVED_ON_TO + 1)),
        "after the restart the partition resumes above the position the store \
         holds now, which is the only place {} could have come from; queries \
         were {after:?}",
        MOVED_ON_TO + 1
    );
}
