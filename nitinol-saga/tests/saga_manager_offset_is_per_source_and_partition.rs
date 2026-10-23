//! The manager records one offset per declared source and per partition.
//!
//! ```gherkin
//!   Scenario: one offset per partition and per source
//!     Given a manager of two partitions observing two sources
//!     When records of both sources are settled
//!     Then saga_offsets holds one key per (identifier, source, partition)
//!          and no other key
//!     And its part is not the delivery host's
//! ```
//!
//! Every partition reads the whole of its source and settles the records of
//! the other partition by owing them nothing, so each of the four keys ends at
//! the last position of its source — a partition held behind the other's
//! records would be stuck below it.
//!
//! The saga decides nothing, so no decision row reaches the delivery host and
//! the keys in the slot are the manager's alone.

#[path = "common/delivery.rs"]
mod delivery;
#[path = "common/manager_offsets.rs"]
mod manager_offsets;
use delivery::{delivery_system, settle, wait_until_stored, RecordingOffsetStore};
use manager_offsets::{
    append_placed, append_shipped, from_stream, handled, made, manager_scope, partitions,
    placed_scope, quiet, shipped_scope, spawn_offset_saga, Acted, OffsetSaga, Placed, Shipped,
    SourceJournal, HOST_ID, MANAGER_PART,
};

use nitinol_eventsource::Event;
use nitinol_saga::{partition_of, SagaDeliveryId, SagaManagerProps};

const PLACED: &str = "manager-keys-placed";
const SHIPPED: &str = "manager-keys-shipped";

/// An instance in partition 0 of two.
const IN_PARTITION_0: &str = "instance-1";

/// An instance in partition 1 of two.
const IN_PARTITION_1: &str = "instance-2";

/// The last stream sequence of each source.
const LAST: u64 = 2;

/// The part of a rendered key: the segment after the reserved namespace.
fn part_of(key: &str) -> &str {
    key.split('.')
        .nth(1)
        .expect("a framework-owned key carries a part after its namespace")
}

#[tokio::test]
async fn each_source_and_partition_records_under_its_own_key_and_no_other() {
    assert_eq!(
        (
            partition_of(IN_PARTITION_0, partitions(2)),
            partition_of(IN_PARTITION_1, partitions(2)),
        ),
        (0, 1),
        "the fixture instances must fall in different partitions of two"
    );

    // Given
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, PLACED, 1, IN_PARTITION_0, "p-1").await;
    append_placed(&journal, PLACED, LAST, IN_PARTITION_1, "p-2").await;
    append_shipped(&journal, SHIPPED, 1, IN_PARTITION_1, "s-1").await;
    append_shipped(&journal, SHIPPED, LAST, IN_PARTITION_0, "s-2").await;

    // When
    let handled = handled();
    let made = made();
    let _host = spawn_offset_saga(
        &system,
        from_stream(PLACED),
        from_stream(SHIPPED),
        2,
        &handled,
        &made,
    )
    .await;

    // Then
    let expected = [
        placed_scope(0),
        placed_scope(1),
        shipped_scope(0),
        shipped_scope(1),
    ];
    for scope in &expected {
        assert_eq!(
            wait_until_stored(
                &offsets,
                scope,
                LAST,
                "every partition of every source records its progress"
            )
            .await,
            LAST,
            "{} ends at its source's last position — its own record by \
             settling it, the other partition's by owing it nothing",
            scope.as_str()
        );
    }

    settle().await;

    let mut want: Vec<String> = expected
        .iter()
        .map(|scope| scope.as_str().to_owned())
        .collect();
    want.sort();
    assert_eq!(
        offsets.keys(),
        want,
        "the slot holds exactly one key per (identifier, source, partition)"
    );
    let delivery_part = part_of(SagaDeliveryId::new(HOST_ID).offset_scope(0).as_str()).to_owned();
    for key in offsets.keys() {
        assert_eq!(
            part_of(&key),
            MANAGER_PART,
            "a manager key is recorded under the manager's part: {key}"
        );
        assert_ne!(
            part_of(&key),
            delivery_part,
            "a manager key never shares the delivery host's part, so one \
             table holds both without a collision: {key}"
        );
    }
}

/// The identifier half of the key follows the host's identifier: with none
/// named, it is the path of the saga's own event type, the same default the
/// delivery host is named by.
#[tokio::test]
async fn an_unnamed_host_records_under_its_event_types_path() {
    // Given
    let journal = SourceJournal::recording();
    let offsets = RecordingOffsetStore::new();
    let system = delivery_system(&journal, &offsets).await;
    append_placed(&journal, PLACED, 1, IN_PARTITION_0, "p-1").await;

    // When
    let handled = handled();
    let _host =
        SagaManagerProps::<OffsetSaga>::new(move |_components| OffsetSaga::recording(&handled))
            .observing::<Placed>(from_stream(PLACED))
            .observing::<Shipped>(quiet())
            .spawn(&system)
            .await;

    // Then
    let default_id = Acted::EVENT_TYPE.to_path().to_string();
    let scope = manager_scope(&default_id, Placed::EVENT_TYPE, 0);
    wait_until_stored(
        &offsets,
        &scope,
        1,
        "an unnamed host records under the default identifier",
    )
    .await;

    settle().await;

    assert_eq!(
        offsets.keys(),
        vec![scope.as_str().to_owned()],
        "one key, for the one source that had a record to settle; the quiet \
         source settled nothing and so recorded nothing"
    );
}
