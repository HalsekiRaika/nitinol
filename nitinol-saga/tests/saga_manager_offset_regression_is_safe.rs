//! An offset that goes backwards re-reads the upstream, and decides nothing
//! twice.
//!
//! ```gherkin
//!   Scenario: a regressed offset is safe
//!     Given a manager whose records are settled has its offset removed from
//!           the store (or its partition count changed)
//!     When the saga is hosted again
//!     Then the upstream records are read again, but none already consumed
//!          reaches handle
//! ```
//!
//! The offset is a cache of what the journal already says: the truth about
//! which records an instance consumed is the instance's own stream. Losing the
//! cache costs a re-read and nothing else, because each instance drops what its
//! stream names as consumed before `handle` sees it.
//!
//! Each test first shows the re-read actually happens — a successor that
//! resumed from where its predecessor stopped would make the second claim
//! true for no reason.

#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;
use delivery::{delivery_system, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, from_stream, handled, made, partitions, placed_scope, quiet, sequences_of,
    spawn_offset_saga, stop_and_wait, wait_until_handled_times, SourceJournal,
};

use nitinol_saga::partition_of;

const SOURCE: &str = "manager-regress-orders";

/// The records settled before the offset is lost.
const SETTLED: u64 = 2;

/// The record appended after the offset is lost: the barrier that says the
/// re-read is over.
const AFTER: u64 = 3;

/// Settle `SETTLED` records of `SOURCE` addressed to `saga` on a host of one
/// partition, wait until the offset is recorded, and stop the host.
async fn settle_then_stop(
    journal: &SourceJournal,
    offsets: &RecordingOffsetStore,
    saga: &str,
    handled: &manager_offsets::Handled,
) {
    let system = delivery_system(journal, offsets).await;
    for sequence in 1..=SETTLED {
        append_placed(journal, SOURCE, sequence, saga, "consumed").await;
    }
    let first = spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, handled, &made()).await;
    wait_until_stored(
        offsets,
        &placed_scope(0),
        SETTLED,
        "the first host must settle and record both records",
    )
    .await;
    stop_and_wait(&system, &first).await;
}

#[tokio::test]
async fn a_lost_offset_store_re_reads_the_upstream_and_hands_handle_nothing_consumed() {
    // Given: the records are settled, and then the offset store is lost — the
    // successor is wired with a fresh one over the same journal.
    let journal = SourceJournal::recording();
    let saga = "manager-regress-instance";
    let handled = handled();
    settle_then_stop(&journal, &RecordingOffsetStore::new(), saga, &handled).await;
    journal.forget_queries();

    // When
    let fresh_offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &fresh_offsets).await;
    let _second =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made()).await;
    append_placed(&journal, SOURCE, AFTER, saga, "new").await;

    // Then
    wait_until_handled_times(&handled, SOURCE, AFTER, 1, "the new record reaches handle").await;
    assert_eq!(
        journal.asked_from(SOURCE).first().copied(),
        Some(Some(1)),
        "with no offset to resume from, the successor reads the source again \
         from its floor"
    );
    assert_eq!(
        sequences_of(&handled, SOURCE),
        vec![1, 2, AFTER],
        "the re-read records are dropped before handle: each reached it once, \
         by the first host"
    );
}

#[tokio::test]
async fn a_changed_partition_count_re_reads_the_upstream_and_hands_handle_nothing_consumed() {
    // An instance that falls in partition 1 once there are two: partition 1 has
    // never recorded anything, so it reads the source from its floor.
    let saga = "instance-2";
    assert_eq!(
        partition_of(saga, partitions(2)),
        1,
        "the fixture instance must fall in the partition the first host never ran"
    );

    // Given: the records are settled on one partition, and the successor is
    // hosted with two.
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let handled = handled();
    settle_then_stop(&journal, &offsets, saga, &handled).await;
    journal.forget_queries();

    // When
    let system = delivery_system(&journal, &offsets).await;
    let _second =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 2, &handled, &made()).await;
    append_placed(&journal, SOURCE, AFTER, saga, "new").await;

    // Then
    wait_until_handled_times(&handled, SOURCE, AFTER, 1, "the new record reaches handle").await;
    assert!(
        journal.asked_from(SOURCE).contains(&Some(1)),
        "the partition with no recorded offset reads the source again from its \
         floor; asked from {:?}",
        journal.asked_from(SOURCE)
    );
    assert_eq!(
        sequences_of(&handled, SOURCE),
        vec![1, 2, AFTER],
        "the re-read records are dropped before handle: each reached it once, \
         by the first host"
    );
}
