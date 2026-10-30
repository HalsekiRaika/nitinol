//! What the index asks the store for.
//!
//! ```gherkin
//!   Scenario: the index reads two fact types, each from a cursor of its own
//!     Given a journal holding a schedule fact and, after it, a lifecycle fact
//!     When the index polls
//!     Then it asks for the schedule facts and the lifecycle facts by their
//!          own event types, across every stream, each above the position it
//!          has itself read up to
//!     And it never asks for a prefix the consumed facts fall under
//!
//!   Scenario: a row of another partition is passed over, not re-read
//!     Given two partitions and one schedule fact, which only one of them owns
//!     When both poll
//!     Then both move past that fact
//! ```
//!
//! Every saga type's framework facts share one family, and the consumed facts
//! grow with every upstream record a saga ever consumed. Asking by each fact
//! type keeps a rebuild proportional to the schedule and lifecycle facts alone.
//! Two event types are two orders, so each has a cursor of its own: one cursor
//! across both would skip whatever the second query's type committed below the
//! position the first query reached.
//!
//! The queries are read off a journal that records what it was handed, so what
//! is fixed is the request itself.

use super::common;
use super::deadline_index;
use common::CONSUMED_MARKER;
use deadline_index::{
    fired, is_lifecycle_load, is_schedule_load, now_millis, probe_system, seed_ended,
    seed_scheduled, spawn_index_saga, wait_until_queries, ProbeJournal, REMINDER,
};

use std::time::Duration;

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::LoadQuery;
use nitinol_saga::{SagaId, Subscription};

/// Far enough out that no deadline falls due while the test runs.
const FAR_OUT: Duration = Duration::from_secs(3_600);

fn positions(queries: &[LoadQuery]) -> Vec<Option<u64>> {
    queries
        .iter()
        .map(|query| query.from_global_sequence)
        .collect()
}

#[tokio::test]
async fn the_index_asks_for_each_fact_type_above_its_own_position() {
    // Given
    let probe = ProbeJournal::recording(InMemoryEventStore::default());
    let journal = probe.journal();
    let scheduled_at = seed_scheduled(
        &journal,
        &SagaId::new("deadline-index-query-scheduled"),
        1,
        REMINDER,
        FAR_OUT,
        "pending",
        now_millis(),
    )
    .await;
    let ended_at = seed_ended(&journal, &SagaId::new("deadline-index-query-ended"), 1).await;
    assert!(
        scheduled_at < ended_at,
        "the fixture's lifecycle fact must sit above its schedule fact"
    );
    let system = probe_system(&probe).await;

    // When
    let _host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-query-unused-upstream"),
        1,
        &fired(),
    )
    .await;
    let schedule = wait_until_queries(&probe, is_schedule_load, 4, "the index must poll").await;
    let lifecycle = wait_until_queries(&probe, is_lifecycle_load, 4, "the index must poll").await;

    // Then
    for query in schedule.iter().chain(&lifecycle) {
        assert!(
            query.stream_key.is_none(),
            "the index reads every saga's stream of this type, so it names no \
             single one; it asked {query:?}"
        );
        assert!(
            query.from_global_sequence.is_some(),
            "the index reads in global order from a position; it asked {query:?}"
        );
    }

    let schedule_positions = positions(&schedule);
    assert_eq!(
        schedule_positions.first(),
        Some(&Some(1)),
        "a fresh index reads the schedule facts from the start of the journal; \
         it asked from {schedule_positions:?}"
    );
    assert!(
        schedule_positions[1..]
            .iter()
            .all(|position| *position == Some(scheduled_at + 1)),
        "once the schedule fact is read the schedule query asks above it, and \
         the lifecycle fact above that is no reason to move: it is read by a \
         cursor of its own; the schedule query asked from {schedule_positions:?}"
    );

    let lifecycle_positions = positions(&lifecycle);
    assert_eq!(
        lifecycle_positions.first(),
        Some(&Some(1)),
        "a fresh index reads the lifecycle facts from the start of the journal; \
         it asked from {lifecycle_positions:?}"
    );
    assert!(
        lifecycle_positions[1..]
            .iter()
            .all(|position| *position == Some(ended_at + 1)),
        "once the lifecycle fact is read the lifecycle query asks above it; it \
         asked from {lifecycle_positions:?}"
    );

    let consumed = CONSUMED_MARKER.to_path();
    let covering: Vec<LoadQuery> = probe
        .queries()
        .into_iter()
        .filter(|query| {
            query
                .event_type_prefix
                .as_ref()
                .is_some_and(|prefix| consumed.is_within(prefix))
        })
        .collect();
    assert!(
        covering.is_empty(),
        "no load may name a prefix the consumed facts fall under, or a rebuild \
         would grow with every upstream record ever consumed; asked {covering:?}"
    );
}

#[tokio::test]
async fn every_partition_moves_past_a_fact_another_partition_owns() {
    // Given
    let probe = ProbeJournal::recording(InMemoryEventStore::default());
    let journal = probe.journal();
    let scheduled_at = seed_scheduled(
        &journal,
        &SagaId::new("deadline-index-query-one-owner"),
        1,
        REMINDER,
        FAR_OUT,
        "owned-by-one",
        now_millis(),
    )
    .await;
    let system = probe_system(&probe).await;
    let partitions = 2;

    // When
    let _host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-query-partition-unused-upstream"),
        partitions,
        &fired(),
    )
    .await;
    let schedule = wait_until_queries(
        &probe,
        is_schedule_load,
        4 * partitions,
        "both partitions must poll",
    )
    .await;

    // Then
    let schedule_positions = positions(&schedule);
    let from_start = schedule_positions
        .iter()
        .filter(|position| **position == Some(1))
        .count();
    assert_eq!(
        from_start, partitions,
        "each partition reads from the start once, when it begins; it asked \
         from {schedule_positions:?}"
    );
    assert!(
        schedule_positions
            .iter()
            .all(|position| *position == Some(1) || *position == Some(scheduled_at + 1)),
        "the partition that does not own the fact passes over it as the owner \
         reads it, so neither asks for it again; asked from {schedule_positions:?}"
    );
}
