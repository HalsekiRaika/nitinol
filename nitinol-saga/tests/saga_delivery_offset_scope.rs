//! The key a delivery host records under is one the framework named.
//!
//! ```gherkin
//!   Scenario: the delivery host records under a framework-owned key
//!     Given a delivery host started with an identifier and a partition number
//!     When it records its progress
//!     Then the key it wrote is inside the framework's reserved namespace
//!     And it names the delivery identifier and the partition number
//! ```
//!
//! What a framework-owned key *is* — that it renders inside the reserved
//! namespace, that no user-named scope can be named one, and that the part
//! name, the name and the partition number render one key injectively — belongs
//! to `OffsetScope` and is fixed by
//! `nitinol-persistence/tests/offset_scope.rs`. What is left here is this
//! crate's own share of it: that the delivery host asks for a framework-owned
//! key at all, rather than rendering a key of its own.
//!
//! The key the host writes is read back through `scope_of`, which is the
//! fixture's name for `SagaDeliveryId::offset_scope` — the entry point tests,
//! examples and operators reach a host's recorded position through. Reading the
//! host's own write through it is what shows the two are one key rather than two
//! renderings that happen to agree.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, settle, spawn_host, wait_until_stored,
    DeliverySaga, RecordingOffsetStore, TEST_DELIVERY_ID,
};

use nitinol_persistence::is_within_reserved_namespace;
use nitinol_saga::SagaId;

/// The key is composed before anything runs, so this half needs no host.
#[test]
fn the_key_a_partition_asks_for_is_framework_owned() {
    for partition in [0usize, 1, 10] {
        let key = scope_of(partition).as_str().to_owned();

        assert!(
            is_within_reserved_namespace(&key),
            "a delivery partition asks `OffsetScope` for a framework-owned key, \
             which renders inside the framework's own namespace; partition \
             {partition} rendered {key:?}"
        );
        assert!(
            key.contains(TEST_DELIVERY_ID),
            "the key names the host the position belongs to, so two hosts over \
             one saga type keep separate progress; got {key:?}"
        );
        assert!(
            key.ends_with(&format!(".{partition}")),
            "the key names which partition of that host it is, so partitions do \
             not overwrite each other; got {key:?}"
        );
    }

    assert_ne!(
        scope_of(0),
        scope_of(1),
        "two partitions of one host record under keys that differ"
    );
}

/// And the running host writes under exactly that key, rather than one it
/// rendered itself.
#[tokio::test]
async fn the_host_records_under_the_key_it_asked_for() {
    let offsets = RecordingOffsetStore::new();
    let (system, journal) = in_memory_system(&offsets).await;

    let last = append_decided(
        &journal,
        &SagaId::new("instance-1"),
        1,
        "ledger-offset-scope",
        "op-a",
    )
    .await;

    let _host = spawn_host::<DeliverySaga>(&system, 1).await;

    wait_until_stored(
        &offsets,
        &scope_of(0),
        last,
        "the host records its progress under the key it composed at spawn",
    )
    .await;

    settle().await;

    assert_eq!(
        offsets.keys(),
        vec![scope_of(0).as_str().to_owned()],
        "the host holds its position under the framework-owned key and under no \
         other, so nothing of it is filed under a key a deployment could name"
    );
}
