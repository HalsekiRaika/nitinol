//! A source whose record cannot be settled holds back its own offset and no
//! other source.
//!
//! ```gherkin
//!   Scenario: one source's stall does not stop another
//!     Given a record of source A that its instance cannot settle (Err)
//!     When a record of source B arrives
//!     Then source B's record reaches handle, and source A's offset stays
//!          below the stalled record
//! ```
//!
//! The stalled record is refused by `handle` and its position cannot be
//! written either, so the instance answers with an error and the partition
//! hands the same record again on a later turn. Each redelivery is a turn that
//! has ended, which is what lets the test say the offset was not moved past
//! the record rather than that it had not been moved yet.

#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;
use delivery::{delivery_system, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, append_shipped, from_stream, handled, made, placed_scope, shipped_scope,
    spawn_offset_saga, wait_until_handled_times, SourceJournal, REFUSE,
};

const SOURCE_A: &str = "manager-stall-a";
const SOURCE_B: &str = "manager-stall-b";

/// The instance source A's first record is addressed to, which settles it.
const FLOWING_A: &str = "manager-stall-flowing-a";

/// The instance the stalled record is addressed to, whose own stream the
/// journal refuses: neither a decision nor a position can land there.
const STALLED: &str = "manager-stall-stalled";

/// The instance source B's record is addressed to.
const FLOWING_B: &str = "manager-stall-flowing-b";

/// Source A's settled record, below the stalled one.
const SETTLED_A: u64 = 1;

/// Source A's record that cannot be settled.
const STALLED_A: u64 = 2;

#[tokio::test]
async fn a_stalled_source_holds_its_own_offset_and_not_the_other_sources_progress() {
    // Given
    let journal = SourceJournal::refusing_appends_to(STALLED);
    journal.refuse(true);
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, SOURCE_A, SETTLED_A, FLOWING_A, "settles").await;
    append_placed(&journal, SOURCE_A, STALLED_A, STALLED, REFUSE).await;

    let handled = handled();
    let made = made();
    let _host = spawn_offset_saga(
        &system,
        from_stream(SOURCE_A),
        from_stream(SOURCE_B),
        1,
        &handled,
        &made,
    )
    .await;

    // When
    append_shipped(&journal, SOURCE_B, 1, FLOWING_B, "flows").await;

    // Then: source B's record is handled and recorded.
    wait_until_handled_times(&handled, SOURCE_B, 1, 1, "source B is not held back").await;
    wait_until_stored(
        &offsets,
        &shipped_scope(0),
        1,
        "source B records its progress while source A is stalled",
    )
    .await;

    // And: source A recorded what it settled, and the stalled record has come
    // round again, so at least one turn has ended after it was first refused.
    wait_until_stored(
        &offsets,
        &placed_scope(0),
        SETTLED_A,
        "source A records the record it did settle",
    )
    .await;
    wait_until_handled_times(
        &handled,
        SOURCE_A,
        STALLED_A,
        2,
        "the stalled record is handed again on a later turn",
    )
    .await;
    assert_eq!(
        offsets.stored(&placed_scope(0)),
        Some(SETTLED_A),
        "source A's offset stays just below the record it cannot settle"
    );
}
