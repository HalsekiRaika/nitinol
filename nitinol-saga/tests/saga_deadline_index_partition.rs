//! Each index partition holds the deadlines of the sagas `partition_of` assigns
//! it, the same assignment the delivery host makes.
//!
//! ```gherkin
//!   Scenario: deadlines split by partition
//!     Given pending deadlines on several saga streams
//!     When the saga type is hosted with two partitions
//!     Then each index partition rebuilds once, holding exactly the deadlines
//!          of the sagas partition_of(saga id, 2) assigns it
//! ```
//!
//! The deadlines are far out, so nothing is asked for and the rebuild reports
//! are the whole of what the partitions say.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    field, fired, now_millis, partition_count, seed_scheduled, spawn_index_saga, system_over,
    Capture, COUNT, PARTITION, REBUILT, REMINDER,
};

use std::collections::BTreeMap;
use std::time::Duration;

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{partition_of, SagaId, Subscription};

const PARTITIONS: usize = 2;

const SAGAS: [&str; 7] = [
    "deadline-index-partition-a",
    "deadline-index-partition-b",
    "deadline-index-partition-c",
    "deadline-index-partition-d",
    "deadline-index-partition-e",
    "deadline-index-partition-f",
    "deadline-index-partition-g",
];

/// Far enough out that no deadline falls due while the test runs.
const FAR_OUT: Duration = Duration::from_secs(3_600);

#[tokio::test]
async fn each_index_partition_holds_the_deadlines_partition_of_assigns_it() {
    // Given
    let capture = Capture::installed();
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    let anchor = now_millis();
    let mut per_partition = vec![0usize; PARTITIONS];
    for saga in SAGAS {
        seed_scheduled(
            &journal,
            &SagaId::new(saga),
            1,
            REMINDER,
            FAR_OUT,
            saga,
            anchor,
        )
        .await;
        per_partition[partition_of(saga, partition_count(PARTITIONS))] += 1;
    }
    let expected: BTreeMap<String, String> = per_partition
        .iter()
        .enumerate()
        .map(|(partition, count)| (partition.to_string(), count.to_string()))
        .collect();
    assert!(
        per_partition.iter().all(|count| *count > 0),
        "the fixture's saga ids must land in every partition for the split to \
         be observable; partition_of assigned {per_partition:?}"
    );

    // When
    let _host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-partition-unused-upstream"),
        PARTITIONS,
        &fired(),
    )
    .await;

    // Then
    let rebuilt = capture
        .wait_for(
            REBUILT,
            |records| records.len() >= PARTITIONS,
            "every index partition must report its rebuild",
        )
        .await;
    let reported: BTreeMap<String, String> = rebuilt
        .iter()
        .filter_map(|fields| {
            Some((
                field(fields, PARTITION)?.to_owned(),
                field(fields, COUNT)?.to_owned(),
            ))
        })
        .collect();
    assert_eq!(
        rebuilt.len(),
        PARTITIONS,
        "each partition rebuilds once; the capture held {rebuilt:?}"
    );
    assert_eq!(
        reported, expected,
        "a partition holds the deadlines of exactly the sagas partition_of \
         assigns it, so the delivery host and the index agree on who owns a \
         saga; the capture held {rebuilt:?}"
    );
}
