//! Progress is recorded per partition, not per host.
//!
//! ```gherkin
//!   Scenario: two partitions, two recorded positions
//!     Given a delivery host of two partitions
//!     When both partitions deliver
//!     Then each records its progress under its own key
//!     And the host holds no position that is not a partition's
//! ```
//!
//! A host that recorded one position for all its partitions would have them
//! overwrite each other's progress, and a held partition would drag the others
//! back to its own position on restart. The key is the identifier together with
//! the partition number, which is what keeps the two apart in one shared table.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, settle, spawn_host, wait_until_stored, Ledger,
    RecordingOffsetStore, SelectiveSaga,
};

use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn each_partition_records_under_its_own_key() {
    // Given: one instance per partition, both delivering to receivers that
    // accept.
    let offsets = RecordingOffsetStore::new();
    let (system, journal) = in_memory_system(&offsets).await;

    let first_target = AggregateId::new("ledger-per-partition-a");
    let second_target = AggregateId::new("ledger-per-partition-b");

    append_decided(
        &journal,
        &SagaId::new("instance-1"),
        1,
        first_target.as_str(),
        "op-a",
    )
    .await;
    let last = append_decided(
        &journal,
        &SagaId::new("instance-2"),
        1,
        second_target.as_str(),
        "op-b",
    )
    .await;

    // When
    let _host = spawn_host::<SelectiveSaga>(&system, 2).await;

    // Then
    for partition in [0usize, 1] {
        assert_eq!(
            wait_until_stored(
                &offsets,
                &scope_of(partition),
                last,
                "each partition must record its own progress"
            )
            .await,
            last,
            "partition {partition} finishes with every record the subscription \
             returned — its own by delivering, the others by owing nothing"
        );
    }

    settle().await;

    assert_eq!(
        offsets.keys(),
        vec![
            scope_of(0).as_str().to_owned(),
            scope_of(1).as_str().to_owned(),
        ],
        "the host holds exactly one position per partition, under keys naming \
         the identifier and the partition number, and no position of its own \
         beside them"
    );

    // The receivers are reached as receivers, so a ledger proxy is enough to
    // say the deliveries were real rather than only recorded as progress.
    let first = system.aggregate_proxy::<Ledger>(first_target);
    let second = system.aggregate_proxy::<Ledger>(second_target);
    assert_eq!(
        delivery::operations_of(&first).await,
        vec!["op-a".to_owned()]
    );
    assert_eq!(
        delivery::operations_of(&second).await,
        vec!["op-b".to_owned()]
    );
}
