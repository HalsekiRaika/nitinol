//! A record whose instance could not record what it decided stops its
//! partition at it.
//!
//! ```gherkin
//!   Scenario: an unsettled record holds its partition and loses nothing
//!     Given a store refusing appends to the saga's own stream, so the
//!           instance fails to append what it decided from record 2
//!           (record 1 is settled, record 3 is waiting upstream)
//!     When ticks go by
//!     Then the instance does not stop and record 2 reaches handle again
//!     And record 3 does not reach handle and the recorded offset stays at 1
//!     When the store accepts appends again
//!     Then record 2 settles, record 3 reaches handle and the offset is 3
//!     And hosting the saga again hands neither record to handle
//!
//!   Scenario: a restart while held re-opens the record
//!     Given the partition is held at record 2
//!     When the saga is stopped and hosted again over a store that accepts
//!          appends
//!     Then the new host resumes from offset 1 and record 2 reaches handle
//! ```
//!
//! The failed record is not settled: nothing on the journal claims it. A
//! consumed position is the highest sequence an instance settled, so a later
//! record of the same instance settling would cover the failed one and it
//! would never reach `handle` again. The partition therefore reads nothing
//! past the record, in this incarnation or the next.

use super::delivery;
use super::manager_offsets;
use delivery::{delivery_system, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, from_stream, handled, made, placed_scope, quiet, sequences_of,
    spawn_offset_saga, stop_and_wait, wait_until_handled_times, wait_until_reads, SourceJournal,
};

use std::sync::atomic::Ordering;

const SOURCE: &str = "manager-hold-orders";

/// The one instance every record here is addressed to, and so the one stream
/// whose appends the journal refuses.
const SAGA: &str = "manager-hold-instance";

/// The record settled before the store starts refusing.
const SETTLED: u64 = 1;

/// The record whose instance cannot record what it decided.
const UNSETTLED: u64 = 2;

/// A later record, which must not be read while the one before it is unsettled.
const LATER: u64 = 3;

#[tokio::test]
async fn a_held_partition_reads_nothing_past_the_record_and_loses_none_once_the_store_recovers() {
    // Given: one record settled and recorded.
    let journal = SourceJournal::refusing_appends_to(SAGA);
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, SOURCE, SETTLED, SAGA, "settled").await;

    let handled = handled();
    let made = made();
    let first = spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        SETTLED,
        "the settled record must be recorded before the store starts refusing",
    )
    .await;

    // And: the store refuses whatever the instance appends from here on.
    handled.lock().expect(delivery::FIXTURE_LOCK).clear();
    journal.refuse(true);

    // When
    append_placed(&journal, SOURCE, UNSETTLED, SAGA, "unsettled").await;
    append_placed(&journal, SOURCE, LATER, SAGA, "later").await;

    // Then: the unsettled record is handed to handle again, and the record
    // behind it is not read while it stays unsettled.
    wait_until_handled_times(
        &handled,
        SOURCE,
        UNSETTLED,
        2,
        "the unsettled record must be handed to handle again",
    )
    .await;
    assert_eq!(
        made.load(Ordering::SeqCst),
        1,
        "the instance that failed to append handled the redelivery: it was \
         not stopped and replaced"
    );
    assert!(
        !sequences_of(&handled, SOURCE).contains(&LATER),
        "no record past the unsettled one may reach handle: its settling would \
         cover the unsettled one"
    );
    assert_eq!(
        offsets.stored(&placed_scope(0)),
        Some(SETTLED),
        "the recorded offset stays just below the record the instance could \
         not settle"
    );

    // When: the store accepts appends again.
    journal.refuse(false);

    // Then: the record settles, the one behind it follows, and the offset
    // moves past both.
    wait_until_handled_times(
        &handled,
        SOURCE,
        LATER,
        1,
        "once the unsettled record settles, the record behind it must reach handle",
    )
    .await;
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        LATER,
        "the offset moves past both records once they are settled",
    )
    .await;
    let order = sequences_of(&handled, SOURCE);
    assert_eq!(
        order.last(),
        Some(&LATER),
        "the later record is handled after the unsettled one, in order: {order:?}"
    );
    assert!(
        order[..order.len() - 1].iter().all(|s| *s == UNSETTLED),
        "only the unsettled record was handled before the later one: {order:?}"
    );

    // And: hosting the saga again hands neither record to handle.
    stop_and_wait(&system, &first).await;
    drop(first);
    journal.forget_queries();
    handled.lock().expect(delivery::FIXTURE_LOCK).clear();
    let _second =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;
    let asked = wait_until_reads(
        &journal,
        SOURCE,
        1,
        "the new host must read its source from the recorded offset",
    )
    .await;
    assert_eq!(
        asked.first().copied(),
        Some(Some(LATER + 1)),
        "the new host resumes from the offset past both records"
    );
    assert!(
        sequences_of(&handled, SOURCE).is_empty(),
        "settled records do not reach handle again"
    );
}

#[tokio::test]
async fn a_restart_while_the_partition_is_held_brings_the_unsettled_record_back_to_handle() {
    // Given: one record settled and recorded, then a store refusing appends.
    let journal = SourceJournal::refusing_appends_to(SAGA);
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, SOURCE, SETTLED, SAGA, "settled").await;

    let handled = handled();
    let made = made();
    let first = spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        SETTLED,
        "the settled record must be recorded before the store starts refusing",
    )
    .await;
    journal.refuse(true);
    append_placed(&journal, SOURCE, UNSETTLED, SAGA, "unsettled").await;
    append_placed(&journal, SOURCE, LATER, SAGA, "later").await;
    wait_until_handled_times(
        &handled,
        SOURCE,
        UNSETTLED,
        2,
        "the partition must be held at the unsettled record",
    )
    .await;

    // When: the saga is stopped while held and hosted again over a store that
    // accepts appends.
    stop_and_wait(&system, &first).await;
    drop(first);
    journal.refuse(false);
    journal.forget_queries();
    handled.lock().expect(delivery::FIXTURE_LOCK).clear();
    let _second =
        spawn_offset_saga(&system, from_stream(SOURCE), quiet(), 1, &handled, &made).await;

    // Then: it resumes from the offset just below the unsettled record, and
    // both records reach handle.
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        LATER,
        "once the re-opened records settle, the offset moves past them",
    )
    .await;
    assert_eq!(
        journal.asked_from(SOURCE).first().copied(),
        Some(Some(UNSETTLED)),
        "the new host resumes from the recorded offset, just below the record \
         that was never settled"
    );
    assert_eq!(
        sequences_of(&handled, SOURCE),
        vec![UNSETTLED, LATER],
        "the record the first host could not settle, and the one it never \
         read, reach handle; the settled one does not"
    );
}
