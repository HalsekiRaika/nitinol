//! The way out of a hold an operator has to make a decision about.
//!
//! ```gherkin
//!   Scenario: an operator moves a held partition on
//!     Given a running delivery host with a held partition
//!     When an operator moves that partition's recorded position past the hold
//!     Then the deliveries in the stretch that was passed over are not made
//!     And the partition resumes from the new position
//! ```
//!
//! The host is held and then released without being restarted, and the same
//! handle is used throughout: that is the contract. An operation that wrote the
//! store and left the running partition reading from where it was would appear
//! to work and change nothing until the next restart.
//!
//! Moving the position only ever goes forward. Backwards is not a weaker
//! version of the same operation — it would re-deliver a stretch the host has
//! already finished, and a forward-only backend would refuse it anyway, so it
//! is refused here rather than being allowed to mean different things in
//! different deployments.
//!
//! The recording is a cache of progress and can be rebuilt; this operation is
//! not a rebuild. It declares that the deliveries in the stretch passed over
//! will not be made, which is a judgement about the business and not about the
//! cache.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, operations_of, scope_of, settle, spawn_host, stream_rows,
    wait_until_loads, wait_until_operations, wait_until_stored, Ledger, RecordingCheckpointStore,
    RecordingEventStore, SelectiveSaga, REFUSED_SUFFIX,
};

use std::sync::Arc;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn an_operator_moves_a_held_partition_past_the_hold() {
    // Given: a held partition, and a decision behind it that cannot be reached.
    let (system, aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let checkpoints = RecordingCheckpointStore::new();
    let saga_id = SagaId::new("instance-1");

    let refused_target = format!("escape-target{REFUSED_SUFFIX}");
    let live_target = AggregateId::new("ledger-escape");

    let held = append_decided(&journal, &saga_id, 1, &refused_target, "op-refused").await;
    let behind = append_decided(&journal, &saga_id, 2, live_target.as_str(), "op-behind").await;

    let host = spawn_host::<SelectiveSaga>(&system, &journal, &checkpoints, 1).await;
    wait_until_loads(&recording, 3, "the host must reach the held record").await;
    settle().await;

    let ledger = system.aggregate_proxy::<Ledger>(live_target);
    assert!(
        operations_of(&ledger).await.is_empty(),
        "before the escape, the decision behind the hold is unreachable"
    );
    assert!(
        checkpoints.stored(&scope_of(0)).is_none_or(|at| at < held),
        "before the escape, nothing is recorded as finished"
    );

    // When: the operator moves this partition past the held record, on the
    // host that is still running.
    host.advance_checkpoint(0, held)
        .await
        .expect("moving a held partition forward is the escape this exists for");

    // Then: it resumes, and the stretch passed over is not delivered.
    assert_eq!(
        wait_until_operations(&ledger, 1, "the partition must resume after the escape").await,
        vec!["op-behind".to_owned()],
        "the same partition, without being restarted, now delivers what lay \
         behind the hold"
    );
    assert_eq!(
        wait_until_stored(
            &checkpoints,
            &scope_of(0),
            behind,
            "progress must move on from the new position"
        )
        .await,
        behind,
    );

    settle().await;
    assert!(
        stream_rows(&aggregates, &refused_target).await.is_empty(),
        "the delivery in the stretch that was passed over is omitted rather \
         than retried later"
    );
}

#[tokio::test]
async fn moving_a_partition_backwards_is_refused() {
    // Given: a partition that has finished a record.
    let (system, _aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoints = RecordingCheckpointStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-escape-backwards");

    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let host = spawn_host::<SelectiveSaga>(&system, &journal, &checkpoints, 1).await;
    wait_until_stored(
        &checkpoints,
        &scope_of(0),
        position,
        "the decision must be delivered first",
    )
    .await;

    // When / Then
    assert!(
        host.advance_checkpoint(0, position).await.is_err(),
        "the position is already there, so this moves nothing forward"
    );
    assert!(
        host.advance_checkpoint(0, position - 1).await.is_err(),
        "moving a partition backwards would re-deliver a stretch it has \
         finished, so it is refused rather than silently applied"
    );
    assert_eq!(
        checkpoints.stored(&scope_of(0)),
        Some(position),
        "a refused move changes nothing"
    );
}
