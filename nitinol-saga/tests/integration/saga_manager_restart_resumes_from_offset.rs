//! A restarted manager resumes each source from the offset it recorded.
//!
//! ```gherkin
//!   Scenario: a restart does not re-read the upstream
//!     Given a manager that records its offset has settled several upstream
//!           records and then stops
//!     And no upstream record is added
//!     When the same saga type is hosted again over the same store
//!     Then every partition of every source resumes from its recorded offset,
//!          and asks the store for no upstream record at or below it
//!     And a new upstream record reaches handle
//! ```
//!
//! What is observed is what the restarted manager *asks for*, not what reaches
//! `handle`: an instance drops a record its own journal names as consumed, so a
//! manager that re-read the whole upstream would still hand `handle` nothing
//! old. The query is the only place the difference between resuming and
//! re-reading shows, so a journal decorator records it.

use super::delivery;
use super::manager_offsets;
use delivery::{delivery_system, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, from_stream, handled, made, placed_scope, quiet, sequences_of,
    spawn_offset_saga, stop_and_wait, wait_until_handled_times, SourceJournal,
};

const SOURCE: &str = "manager-resume-orders";
const SAGA: &str = "manager-resume-instance";

/// The records settled before the restart, and so the offset recorded for them.
const SETTLED: u64 = 3;

#[tokio::test]
async fn a_restarted_manager_asks_for_nothing_at_or_below_its_recorded_offset() {
    // Given: a manager that settles three upstream records, records its
    // offset, and stops.
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    for sequence in 1..=SETTLED {
        append_placed(&journal, SOURCE, sequence, SAGA, "before-restart").await;
    }

    let handled = handled();
    let made = made();
    let first = spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;
    wait_until_handled_times(&handled, SOURCE, SETTLED, 1, "the first host settles seq 3").await;
    assert_eq!(
        wait_until_stored(
            &offsets,
            &placed_scope(0),
            SETTLED,
            "the first host must record how far its partition has settled"
        )
        .await,
        SETTLED,
        "the recorded offset is the last settled position, in the stream's own \
         sequence"
    );
    stop_and_wait(&system, &first).await;
    drop(first);
    journal.forget_queries();

    // When: the same saga type is hosted again over the same store.
    let _second =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;
    append_placed(&journal, SOURCE, SETTLED + 1, SAGA, "after-restart").await;

    // Then
    wait_until_handled_times(
        &handled,
        SOURCE,
        SETTLED + 1,
        1,
        "a record appended after the restart must reach handle",
    )
    .await;
    let asked_from = journal.asked_from(SOURCE);

    assert_eq!(
        asked_from.first().copied(),
        Some(Some(SETTLED + 1)),
        "the restarted partition's first read starts just above its recorded \
         offset; asked from {asked_from:?}"
    );
    assert!(
        asked_from
            .iter()
            .all(|from| from.is_some_and(|from| from > SETTLED)),
        "no read of the restarted manager may reach back to a record at or \
         below the offset it recorded; asked from {asked_from:?}"
    );
    assert_eq!(
        sequences_of(&handled, SOURCE),
        vec![1, 2, 3, 4],
        "each record reaches handle once across both hosts: the settled ones \
         by the first, the new one by the second"
    );
}
