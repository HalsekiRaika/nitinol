//! An activation request is repeated until the journal settles the deadline.
//!
//! ```gherkin
//!   Scenario: a due deadline nobody acts on
//!     Given a hosted saga type whose manager has been stopped
//!     When a due deadline is written on a saga's own stream
//!     Then the index asks for that saga's activation, and asks again on a
//!          later tick, because no fired, cancelled or ended fact has removed it
//! ```
//!
//! Asking is at-least-once: the index does not take a deadline out because it
//! asked, only because the journal says the deadline is over. A stopped manager
//! is the simplest party that never answers, and the index partitions are not
//! its children, so they keep asking.
//!
//! Each request names the partition that asked, the saga and timer it is for,
//! and the due time the schedule fact names.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    due_at_millis, field, fired, now_millis, partition_count, seed_scheduled, spawn_index_saga,
    system_over, Capture, Fields, DUE_AT_UNIX_MILLIS, PARTITION, PROBE_INTERVAL, PROBE_TIMEOUT,
    REMINDER, REQUESTED_ACTIVATION, SAGA_ID, TIMER,
};

use std::time::{Duration, Instant};

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{partition_of, SagaId, Subscription};

const PARTITIONS: usize = 2;
const SAGA: &str = "deadline-index-unanswered-1";

#[tokio::test]
async fn a_due_deadline_is_asked_for_again_until_the_journal_settles_it() {
    // Given
    let capture = Capture::installed();
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    let host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-unanswered-upstream"),
        PARTITIONS,
        &fired(),
    )
    .await;
    let ps = system.process_system();
    host.manager()
        .stop()
        .await
        .expect("stopping a running manager must be accepted");
    let stopped_by = Instant::now() + PROBE_TIMEOUT;
    while ps.lookup(host.manager().pid()).await.is_some() {
        assert!(
            Instant::now() < stopped_by,
            "the manager must leave the registry once it is stopped"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }

    // When
    let saga_id = SagaId::new(SAGA);
    let anchor = now_millis() - 10_000;
    let after = Duration::from_millis(500);
    seed_scheduled(&journal, &saga_id, 1, REMINDER, after, "unanswered", anchor).await;

    // Then
    let requests = capture
        .wait_for(
            REQUESTED_ACTIVATION,
            |records| for_saga(records).len() >= 2,
            "a due deadline that nothing settles must be asked for again",
        )
        .await;
    let expected_partition = partition_of(SAGA, partition_count(PARTITIONS)).to_string();
    let expected_due = due_at_millis(anchor, after).to_string();
    for fields in for_saga(&requests) {
        assert_eq!(
            (
                field(fields, PARTITION),
                field(fields, TIMER),
                field(fields, DUE_AT_UNIX_MILLIS),
            ),
            (
                Some(expected_partition.as_str()),
                Some(REMINDER),
                Some(expected_due.as_str()),
            ),
            "a request names the partition partition_of assigns the saga, the \
             timer, and the due time the schedule fact names; it was {fields:?}"
        );
    }
}

fn for_saga(records: &[Fields]) -> Vec<&Fields> {
    records
        .iter()
        .filter(|fields| field(fields, SAGA_ID) == Some(SAGA))
        .collect()
}
