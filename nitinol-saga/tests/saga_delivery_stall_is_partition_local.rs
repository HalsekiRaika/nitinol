//! A held delivery holds its own partition and nothing else.
//!
//! ```gherkin
//!   Scenario: one instance held, another progressing
//!     Given a delivery host of two partitions
//!     And an instance whose delivery is refused again and again
//!     When another instance's decision is persisted into a different partition
//!     Then only the refused instance's partition is held
//!     And the other partition's deliveries go through
//! ```
//!
//! Both instances are the same saga type on the same host, so the only thing
//! separating them is the partition their stream key falls in. A single
//! consumer, or partitions sharing one recorded position, would let the
//! refusal stop the other instance too.
//!
//! The progressing partition also has to move past the held instance's record,
//! which it does not own. Nothing is owed on a record belonging to another
//! partition, so holding the cursor there would make the store re-scan the same
//! tail forever while a refusal somewhere else never clears.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, operations_of, scope_of, settle, spawn_host, stream_rows,
    wait_until_operations, wait_until_stored, Ledger, RecordingCheckpointStore, SelectiveSaga,
    REFUSED_SUFFIX,
};

use std::sync::Arc;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_saga::{partition_of, SagaId};

/// The instance whose delivery is refused. Its key falls in partition 0 of two.
const HELD_INSTANCE: &str = "instance-1";

/// The instance that keeps progressing. Its key falls in partition 1 of two.
const LIVE_INSTANCE: &str = "instance-2";

#[test]
fn the_two_instances_are_in_different_partitions() {
    let two = delivery::partition_count(2);

    assert_eq!(
        partition_of(HELD_INSTANCE, two),
        0,
        "the rest of this file rests on these two keys landing apart"
    );
    assert_eq!(partition_of(LIVE_INSTANCE, two), 1);
}

#[tokio::test]
async fn a_refusal_in_one_partition_leaves_the_other_delivering() {
    // Given
    let (system, aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoints = RecordingCheckpointStore::new();

    let held_target = format!("stall-target{REFUSED_SUFFIX}");
    let live_target = AggregateId::new("ledger-stall-local");

    let held_position = append_decided(
        &journal,
        &SagaId::new(HELD_INSTANCE),
        1,
        &held_target,
        "op-held",
    )
    .await;
    let live_position = append_decided(
        &journal,
        &SagaId::new(LIVE_INSTANCE),
        1,
        live_target.as_str(),
        "op-live",
    )
    .await;

    // When
    let _host = spawn_host::<SelectiveSaga>(&system, &journal, &checkpoints, 2).await;

    // Then: the partition that can deliver does.
    let ledger = system.aggregate_proxy::<Ledger>(live_target);
    assert_eq!(
        wait_until_operations(
            &ledger,
            1,
            "the unaffected partition must deliver while the other is held"
        )
        .await,
        vec!["op-live".to_owned()],
    );
    assert_eq!(
        wait_until_stored(
            &checkpoints,
            &scope_of(1),
            live_position,
            "the progressing partition must record its own progress"
        )
        .await,
        live_position,
        "the progressing partition moves past the held instance's record too: \
         that record belongs to another partition, so this one owes nothing on \
         it"
    );

    settle().await;

    // And: the held partition is still held, on its own recorded position.
    assert!(
        checkpoints
            .stored(&scope_of(0))
            .is_none_or(|position| position < held_position),
        "the refused partition must not record progress past the record it \
         could not deliver"
    );

    assert!(
        stream_rows(&aggregates, &held_target).await.is_empty(),
        "nothing was ever accepted at the refusing receiver"
    );
    assert_eq!(
        operations_of(&ledger).await,
        vec!["op-live".to_owned()],
        "and the held delivery never leaked into the other partition's target"
    );
}
