//! A store that holds a snapshot slot says so by value, too.
//!
//! ```gherkin
//!   Scenario: a store with a snapshot slot answers with that slot's album
//!     Given a store that implements `SnapshotSlot`
//!     When its snapshot slot is asked for through `Store` alone
//!     Then it answers with an album
//!     And that album reaches the records `SnapshotSlot::snapshots` reaches
//! ```
//!
//! Erasing a store's type keeps only what `Store` can say about it, so whether
//! a deployment holds a snapshot slot survives the erasure only if `Store` says
//! it. The rule this fixes is the one-way half: a store that implements
//! `SnapshotSlot` never answers "no slot" here. A store that did would compile,
//! and every `Recover = Latest` aggregate reached through the erased handle
//! would then be refused on a deployment that holds the slot.
//!
//! Answering `Some` is not enough on its own. An album built at the call — a
//! fresh in-memory store, say — would be `Some` and reach nothing the typed
//! slot holds, so each clause saves through the answer and reads back through
//! the typed slot or through the backend handle the store was assembled from.
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test integration snapshot_slot_store_answers_with_its_album::`.

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::provider::{CustomStore, SnapshotSlot, Store};
use nitinol_persistence::store::{
    Album, InMemoryEventStore, InMemorySnapshotStore, InMemoryStore, SnapshotStore,
};
use nitinol_persistence::{AggregateId, PersistedSnapshot};

fn a_snapshot(aggregate_id: &AggregateId, sequence: u64) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(b"captured-state"),
        created_at: Timestamp::now(),
    }
}

/// The sequence `album` holds for `aggregate`, if any.
async fn album_sequence(album: &Album, aggregate: &AggregateId) -> Option<u64> {
    album
        .load_latest(aggregate)
        .await
        .expect("loading from an album succeeds")
        .map(|found| found.sequence)
}

/// The in-memory reference store holds every slot, so it answers with its
/// snapshot slot through `Store` too, and that answer is its slot rather than a
/// new one.
#[tokio::test]
async fn the_in_memory_store_answers_with_the_album_its_snapshot_slot_holds() {
    // Given
    let store = InMemoryStore::default();
    let aggregate = AggregateId::new("slot-by-value.in-memory");

    // When
    let by_value = store.snapshot_slot().expect(
        "a store implementing `SnapshotSlot` must not answer that it holds no \
         snapshot slot",
    );
    by_value
        .save(a_snapshot(&aggregate, 7))
        .await
        .expect("saving through the album answered by value succeeds");

    // Then
    assert_eq!(
        album_sequence(&store.snapshots(), &aggregate).await,
        Some(7),
        "the album answered through `Store` is the snapshot slot the store \
         holds, so a save through it is what `SnapshotSlot::snapshots` reads"
    );
}

/// An assembled store whose snapshot slot was named answers with that slot,
/// and the answer reaches the backend handle it was named with.
#[tokio::test]
async fn an_assembled_store_with_a_named_snapshot_slot_answers_with_that_handle() {
    // Given
    let snapshots = InMemorySnapshotStore::default();
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(snapshots.clone())
        .build();
    let aggregate = AggregateId::new("slot-by-value.assembled");

    // When
    let by_value = store.snapshot_slot().expect(
        "an assembled store whose snapshot slot was named implements \
         `SnapshotSlot`, so it must not answer that it holds none",
    );
    by_value
        .save(a_snapshot(&aggregate, 11))
        .await
        .expect("saving through the album answered by value succeeds");

    // Then
    assert_eq!(
        snapshots
            .load_latest(&aggregate)
            .await
            .expect("loading from the named snapshot handle succeeds")
            .map(|found| found.sequence),
        Some(11),
        "the album answered through `Store` reaches the handle the snapshot \
         slot was named with"
    );
    assert_eq!(
        album_sequence(&store.snapshots(), &aggregate).await,
        Some(11),
        "and it is the album `SnapshotSlot::snapshots` answers with"
    );
}
