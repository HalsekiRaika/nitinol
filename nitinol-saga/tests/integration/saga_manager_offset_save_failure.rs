//! The manager records its progress once per turn, and a save that fails is
//! retried rather than stopping anything.
//!
//! ```gherkin
//!   Scenario: one save per turn
//!     Given several upstream records read by one load
//!     When the partition settles them all
//!     Then it saves its offset once, at the last of them
//!
//!   Scenario: a failed save is retried on a later turn
//!     Given an offset store that refuses the partition's first save
//!     When the partition goes on running
//!     Then the save is made again on a later turn
//!     And the partition goes on delivering from where it had read to
//! ```
//!
//! A save per record would multiply the writes by the length of every
//! catch-up; a save that stopped the partition, or sent it back to re-read what
//! it had already settled, would turn a transient store failure into a stall.

use super::delivery;
use super::manager_offsets;
use delivery::{delivery_system, wait_until_refused, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, from_stream, handled, made, placed_scope, quiet, spawn_offset_saga,
    wait_until_handled_times, wait_until_reads, SourceJournal,
};

const SOURCE: &str = "manager-save-orders";
const SAGA: &str = "manager-save-instance";

/// The records one load returns.
const IN_ONE_LOAD: u64 = 3;

/// The manager's saves, as `(key, sequence)` in call order.
fn manager_saves(offsets: &RecordingOffsetStore) -> Vec<(String, u64)> {
    let key = placed_scope(0).as_str().to_owned();
    offsets
        .save_calls()
        .into_iter()
        .filter(|(saved, _)| *saved == key)
        .collect()
}

#[tokio::test]
async fn records_settled_from_one_load_are_saved_once() {
    // Given: every record is in the store before the partition's first read.
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    for sequence in 1..=IN_ONE_LOAD {
        append_placed(&journal, SOURCE, sequence, SAGA, "batched").await;
    }

    // When
    let _host = spawn_offset_saga(
        &system,
        from_stream(SOURCE),
        quiet(),
        1,
        &handled(),
        &made(),
    )
    .await;
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        IN_ONE_LOAD,
        "the partition records the last record it settled",
    )
    .await;
    // The read after the one that returned the records starts a later turn,
    // so the turn that settled them is over and has made every save it will.
    let reads = journal.asked_from(SOURCE).len();
    wait_until_reads(&journal, SOURCE, reads + 1, "the partition goes on polling").await;

    // Then
    assert_eq!(
        manager_saves(&offsets),
        vec![(placed_scope(0).as_str().to_owned(), IN_ONE_LOAD)],
        "one turn settles every record its load returned and saves once, at \
         the last of them; nothing is saved for a turn that settled nothing"
    );
}

#[tokio::test]
async fn a_refused_save_is_made_again_and_the_partition_reads_on() {
    // Given
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    offsets.fail_next_saves(1);
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, SOURCE, 1, SAGA, "first").await;

    let handled = handled();
    let _host =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made()).await;
    wait_until_refused(&offsets, 1, "the partition's first save must be refused").await;

    // When / Then: a later turn makes the save again, with no new record to
    // prompt it.
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        1,
        "the refused save is made again on a later turn",
    )
    .await;

    // And: the partition delivers on, from where it had read to.
    append_placed(&journal, SOURCE, 2, SAGA, "second").await;
    wait_until_handled_times(&handled, SOURCE, 2, 1, "the partition keeps delivering").await;
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        2,
        "and keeps recording what it settles",
    )
    .await;
    let asked_from = journal.asked_from(SOURCE);
    assert!(
        asked_from
            .iter()
            .skip(1)
            .all(|from| from.is_some_and(|from| from > 1)),
        "a refused save does not send the partition back over what it had \
         already read; asked from {asked_from:?}"
    );
}
