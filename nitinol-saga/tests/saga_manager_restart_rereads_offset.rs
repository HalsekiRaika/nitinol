//! A source partition rebuilt by supervision resumes from what the store holds
//! then.
//!
//! ```gherkin
//!   Scenario: a source partition restarted under supervision
//!     Given a running source partition of a manager
//!     And a recorded offset that has since moved on
//!     When the partition fails and is rebuilt
//!     Then its next read resumes above the offset as it now stands
//! ```
//!
//! The partition lives across the rebuild — it is the one the manager started,
//! supervised — so where it resumes has to be read at the rebuild rather than
//! settled once when the manager was wired. A position captured at the wiring
//! would send the rebuilt partition back to where it began.
//!
//! The source is left empty on purpose: with nothing to settle the partition
//! records nothing of its own, so the only offset in the store is the one put
//! there from outside, and a read above it can only have come from reading it.

#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;
use delivery::{delivery_system, RecordingOffsetStore};
use manager_offsets::{
    handled, made, placed_scope, quiet, spawn_offset_saga, wait_until_reads, SourceJournal,
};

use std::time::Duration;

use nitinol_saga::Subscription;

const SOURCE: &str = "manager-rebuild-orders";

/// Slow enough that the offset below is in the store well before the read
/// that takes the partition down.
const POLL: Duration = Duration::from_millis(200);

/// The read of the source that panics, so the partition is rebuilt.
const FAILING_READ: usize = 3;

/// An offset the partition could not have reached on its own.
const MOVED_ON_TO: u64 = 5;

#[tokio::test]
async fn a_rebuilt_partition_resumes_from_the_offset_now_recorded() {
    // Given
    let journal = SourceJournal::panicking_on_load_of(SOURCE, FAILING_READ);
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    let _host = spawn_offset_saga(
        &system,
        Subscription::stream(SOURCE).with_poll_interval(POLL),
        quiet(),
        1,
        &handled(),
        &made(),
    )
    .await;
    wait_until_reads(&journal, SOURCE, 1, "the partition must start reading").await;

    // And: the recorded offset moves on while the partition is running.
    offsets.put(&placed_scope(0), MOVED_ON_TO);

    // When: a later read takes the partition down, and supervision rebuilds it.
    let asked_from = wait_until_reads(
        &journal,
        SOURCE,
        FAILING_READ + 2,
        "the partition must be rebuilt and read again",
    )
    .await;

    // Then
    assert_eq!(
        asked_from.first().copied(),
        Some(Some(1)),
        "with no offset recorded, the partition first reads from its floor; \
         asked from {asked_from:?}"
    );
    let after = &asked_from[FAILING_READ..];
    assert!(
        after.iter().all(|from| *from == Some(MOVED_ON_TO + 1)),
        "after the rebuild the partition resumes above the offset the store \
         holds now, which is the only place {} could have come from; asked \
         from {asked_from:?}",
        MOVED_ON_TO + 1
    );
}
