//! One instance's decisions are delivered in the order they were persisted.
//!
//! ```gherkin
//!   Scenario: several decisions from one instance
//!     Given a saga instance that persisted three decisions in order
//!     When a partitioned delivery host delivers them
//!     Then they all go through the one partition its stream key falls in
//!     And they arrive in the order they were persisted
//! ```
//!
//! Partitioning by the originating stream key is what makes this possible at
//! all: with every one of an instance's decisions in one partition, keeping
//! the order inside a partition keeps the order of the instance. Delivering the
//! records of one partition concurrently would lose it, and the receiver would
//! see the decisions in an order no saga ever decided.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, spawn_host, wait_until_operations,
    wait_until_stored, DeliverySaga, Ledger, RecordingOffsetStore,
};

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::{partition_of, SagaId};

#[tokio::test]
async fn one_instance_is_delivered_in_persisted_order() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-order");

    for (sequence, operation) in [(1u64, "op-1"), (2, "op-2"), (3, "op-3")] {
        append_decided(&journal, &saga_id, sequence, target.as_str(), operation).await;
    }

    // When: four partitions, so the instance occupies one of several.
    let partitions = 4usize;
    let _host = spawn_host::<DeliverySaga, _>(&system, &journal, &offsets, partitions).await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target);
    let operations =
        wait_until_operations(&ledger, 3, "every decision of the instance must arrive").await;
    assert_eq!(
        operations,
        vec!["op-1".to_owned(), "op-2".to_owned(), "op-3".to_owned()],
        "the receiver records what it carried out in the order it did, so this \
         is the delivery order rather than a reordering of the assertion"
    );

    // And: the partition the instance's stream key falls in is the one that
    // finished its records, so every one of them went through that partition.
    let owner = partition_of(saga_id.as_str(), delivery::partition_count(partitions));
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(owner),
            3,
            "the owning partition must record progress past the last decision"
        )
        .await,
        3,
        "all three records are the owning partition's, so it is the partition \
         whose recorded position reaches the last of them"
    );
}
