//! A recorded offset below the subscription's floor does not undercut it.
//!
//! ```gherkin
//!   Scenario: the floor holds
//!     Given a recorded offset smaller than the with_after floor
//!     When the saga is hosted
//!     Then no record at or below the floor is delivered
//! ```
//!
//! `with_after` is a floor and the offset is where to resume, so the manager
//! resumes from whichever is higher. An offset preferred over the floor would
//! hand `handle` the records the floor names as never to be delivered, and none
//! of them is consumed, so no instance would drop them.

#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;
use delivery::{delivery_system, RecordingOffsetStore};
use manager_offsets::{
    append_placed, from_stream, handled, made, placed_scope, quiet, sequences_of,
    spawn_offset_saga, wait_until_handled_times, SourceJournal,
};

const SOURCE: &str = "manager-floor-orders";
const SAGA: &str = "manager-floor-instance";

/// The offset found in the store when the host starts.
const RECORDED: u64 = 1;

/// The subscription's floor, above the recorded offset.
const FLOOR: u64 = 3;

/// The one record above the floor.
const ABOVE_FLOOR: u64 = 4;

#[tokio::test]
async fn a_recorded_offset_below_the_floor_delivers_nothing_at_or_below_the_floor() {
    // Given
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    for sequence in 1..=ABOVE_FLOOR {
        append_placed(&journal, SOURCE, sequence, SAGA, "unconsumed").await;
    }
    offsets.put(&placed_scope(0), RECORDED);

    // When
    let handled = handled();
    let made = made();
    let _host = spawn_offset_saga(
        &system,
        from_stream(SOURCE).with_after(FLOOR),
        quiet(),
        1,
        &handled,
        &made,
    )
    .await;

    // Then: the record above the floor is the barrier. The partition reads in
    // ascending order and hands one record at a time, so once it has reached
    // `handle` every record below it has already been judged.
    wait_until_handled_times(
        &handled,
        SOURCE,
        ABOVE_FLOOR,
        1,
        "the record above the floor must be delivered",
    )
    .await;

    assert_eq!(
        sequences_of(&handled, SOURCE),
        vec![ABOVE_FLOOR],
        "seq 2 and 3 sit above the recorded offset but at or below the floor, \
         and the floor is never undercut"
    );
    assert_eq!(
        journal.asked_from(SOURCE).first().copied(),
        Some(Some(FLOOR + 1)),
        "the partition resumes from the higher of the floor and the recorded \
         offset"
    );
}
