//! An erased store holds the journal and, by value, the snapshot slot — and
//! implements no slot trait.
//!
//! ```gherkin
//!   Scenario: an erased store reaches what the store it was taken from holds
//!     Given a store that holds a journal and a snapshot slot
//!     When it is erased, and the erased store is cloned
//!     Then a fact appended through either erased journal is what the original
//!          journal loads
//!     And a snapshot saved through either erased album is what the original
//!          snapshot slot reads
//!
//!   Scenario: an erased store implements no slot trait
//!     Given an erased store
//!     When it is asked whether it implements each slot trait
//!     Then it implements none of `SnapshotSlot`, `ProjectionOffsetSlot` and
//!          `SagaOffsetSlot`
//! ```
//!
//! The erased store is what keeps a deployment's snapshot slot through an
//! erasure: the store's type is gone, and whether it held the slot is a value.
//! Values are checked when the program runs, not when it is compiled. If the
//! erased store implemented `SnapshotSlot` so that some wiring compiled, that
//! wiring would be given a slot it could not tell was missing. So the slot is
//! readable only through `Store`. The trait impls stay absent, and wiring that
//! needs a slot keeps refusing an erased store when it is compiled, which is
//! `PRV-3`.
//!
//! The absence is read without failing the build. A probe type has an
//! inherent constant behind each slot trait's bound and a blanket trait
//! constant that applies otherwise. Inherent items win where their bound holds,
//! so each constant answers whether the bound holds for the type named. The
//! in-memory store is read through the same probe, so an answer of "absent"
//! cannot be explained by a probe that only ever says "absent".
//!
//! Run with:
//! `cargo test -p nitinol-persistence --test integration erased_store_carries_only_the_snapshot_slot::`.

use std::marker::PhantomData;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::provider::{
    ErasedStore, ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::{Album, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot, TypeName,
};

const A_FACT: EventType = EventType::new(Family::new("erased.store"), TypeName::new("Fact"));

/// Append one fact to `key` at `sequence` through `journal`.
async fn append_to(journal: &Journal, key: &AggregateId, sequence: u64, payload: &'static [u8]) {
    journal
        .append(
            key.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: A_FACT,
                payload: Bytes::from_static(payload),
                occurred_at: Timestamp::now(),
            }],
        )
        .await
        .expect("appending through a journal succeeds");
}

/// The payloads `journal` holds under `key`, in stored order.
async fn payloads_of(journal: &Journal, key: &AggregateId) -> Vec<Bytes> {
    let loaded: Vec<_> = journal
        .load(LoadQuery::by_stream(key))
        .await
        .expect("loading a stream of a journal succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of a journal succeeds");
    loaded.into_iter().map(|event| event.payload).collect()
}

/// Save a snapshot of `aggregate` at `sequence` through `album`.
async fn save_at(album: &Album, aggregate: &AggregateId, sequence: u64) {
    album
        .save(PersistedSnapshot {
            aggregate_id: aggregate.clone(),
            sequence,
            payload: Bytes::from_static(b"captured-state"),
            created_at: Timestamp::now(),
        })
        .await
        .expect("saving through an album succeeds");
}

/// The sequence `album` holds for `aggregate`, if any.
async fn album_sequence(album: &Album, aggregate: &AggregateId) -> Option<u64> {
    album
        .load_latest(aggregate)
        .await
        .expect("loading from an album succeeds")
        .map(|found| found.sequence)
}

/// Whether a type implements each slot trait, read as a constant.
struct Implements<T>(PhantomData<T>);

/// The answer wherever an inherent constant's bound does not hold.
trait Absent {
    const SNAPSHOT_SLOT: bool = false;
    const PROJECTION_OFFSET_SLOT: bool = false;
    const SAGA_OFFSET_SLOT: bool = false;
}

impl<T> Absent for Implements<T> {}

impl<T: SnapshotSlot> Implements<T> {
    const SNAPSHOT_SLOT: bool = true;
}

impl<T: ProjectionOffsetSlot> Implements<T> {
    const PROJECTION_OFFSET_SLOT: bool = true;
}

impl<T: SagaOffsetSlot> Implements<T> {
    const SAGA_OFFSET_SLOT: bool = true;
}

/// The erased journal and the erased album both reach the store they were
/// taken from, and so does a clone of the erased store.
///
/// An erased store that built a journal or an album of its own would still
/// answer, but the facts and snapshots written through it would not reach the
/// store it was taken from, so every read below goes through the original
/// store.
#[tokio::test]
async fn an_erased_store_and_its_clone_reach_the_journal_and_snapshot_slot_of_the_original() {
    // Given
    let original = InMemoryStore::default();
    let erased = ErasedStore::of(&original);
    let copy = erased.clone();
    let stream = AggregateId::new("erased.store.stream");
    let through_erased = AggregateId::new("erased.store.snapshot.erased");
    let through_copy = AggregateId::new("erased.store.snapshot.copy");

    // When
    append_to(&erased.events(), &stream, 1, b"through-the-erased-store").await;
    append_to(&copy.events(), &stream, 2, b"through-its-clone").await;
    save_at(
        &erased
            .snapshot_slot()
            .expect("erasing a store that holds a snapshot slot keeps that slot"),
        &through_erased,
        3,
    )
    .await;
    save_at(
        &copy
            .snapshot_slot()
            .expect("a clone of an erased store keeps the slot it carries"),
        &through_copy,
        5,
    )
    .await;

    // Then
    assert_eq!(
        payloads_of(&original.events(), &stream).await,
        vec![
            Bytes::from_static(b"through-the-erased-store"),
            Bytes::from_static(b"through-its-clone"),
        ],
        "the erased store's journal and its clone's are the journal of the store \
         it was taken from"
    );
    assert_eq!(
        album_sequence(&original.snapshots(), &through_erased).await,
        Some(3),
        "the album the erased store carries is the snapshot slot of the store it \
         was taken from"
    );
    assert_eq!(
        album_sequence(&original.snapshots(), &through_copy).await,
        Some(5),
        "and its clone carries that same album"
    );
}

/// An erased store implements none of the slot traits, while the store it was
/// taken from implements all three.
#[test]
fn an_erased_store_implements_no_slot_trait() {
    let control = [
        ("SnapshotSlot", Implements::<InMemoryStore>::SNAPSHOT_SLOT),
        (
            "ProjectionOffsetSlot",
            Implements::<InMemoryStore>::PROJECTION_OFFSET_SLOT,
        ),
        (
            "SagaOffsetSlot",
            Implements::<InMemoryStore>::SAGA_OFFSET_SLOT,
        ),
    ];
    let unseen: Vec<&str> = control
        .iter()
        .filter(|(_, implements)| !*implements)
        .map(|(name, _)| *name)
        .collect();
    assert!(
        unseen.is_empty(),
        "the probe must answer \"implements\" for a store holding every slot, or \
         the answers below say nothing; it missed {unseen:?}"
    );

    let answered = [
        ("SnapshotSlot", Implements::<ErasedStore>::SNAPSHOT_SLOT),
        (
            "ProjectionOffsetSlot",
            Implements::<ErasedStore>::PROJECTION_OFFSET_SLOT,
        ),
        (
            "SagaOffsetSlot",
            Implements::<ErasedStore>::SAGA_OFFSET_SLOT,
        ),
    ];
    let implemented: Vec<&str> = answered
        .iter()
        .filter(|(_, implements)| *implements)
        .map(|(name, _)| *name)
        .collect();

    assert!(
        implemented.is_empty(),
        "an erased store must implement no slot trait: whether it holds a \
         snapshot slot is a value checked when the program runs, and wiring that \
         needs a slot keeps refusing it when compiled (`PRV-3`); it implements \
         {implemented:?}"
    );
}
