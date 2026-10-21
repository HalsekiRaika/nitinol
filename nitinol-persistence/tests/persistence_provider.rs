//! What a store and the supplier that distributes it owe about the records
//! their handles reach.
//!
//! ```gherkin
//!   Scenario: a store handle's clone reaches the same records
//!     Given a store configured as one logical destination
//!     When a clone of the store handle is taken
//!     Then a record written through one handle is read through the other
//!
//!   Scenario: distribution is idempotent
//!     Given a slot taken twice from one supplier
//!     When a record is written through the first handle
//!     Then the second handle answers with it
//!
//!   Scenario: a fresh store is a fresh data region
//!     Given two stores each constructed as a new destination
//!     When a record is written through one
//!     Then the other holds nothing
//!
//!   Scenario: an assembled store answers for the handles it was named with
//!     Given a store assembled from handles the caller still holds
//!     When a record is written through the supplier's handle
//!     Then the handle the caller holds is where it landed
//!
//!   Scenario: two offset slots over one handle are one data region
//!     Given a store whose offset slots were named with one handle
//!     When a position is saved through the projection slot
//!     Then the saga slot answers with it
//!
//!   Scenario: two offset slots named apart are two data regions
//!     Given a store whose offset slots were named with two handles
//!     When a position is saved through the projection slot
//!     Then the saga slot holds no position
//!     And a later save through the saga slot leaves the projection slot's
//!         answer where it was
//!
//!   Scenario: the journal a store answers with is a `Journal`
//!     Given a store assembled over a backend handed over as itself
//!     When the supplier's journal is taken
//!     Then it is a `Journal`, with no `Arc` written anywhere in the wiring
//!     And a fact appended through it is loaded through a second one
//!
//!   Scenario: a journal's clone is that journal
//!     Given a `Journal` over a backend
//!     When it is cloned
//!     Then a fact appended through one is loaded through the other
//!
//!   Scenario: taking a journal by reference is cloning it
//!     Given a `Journal` over a backend
//!     When one handle is taken with `Journal::from(&journal)` and another with
//!          `journal.clone()`
//!     Then both reach the records the original holds
//!
//!   Scenario: a store holding the journal alone is the journal itself
//!     Given a `Journal` handed to `PersistenceProvider::new`
//!     When the supplier's journal is taken
//!     Then it reaches the records the journal it was built from holds
//! ```
//!
//! Every clause reads back through a *second* handle, or through the handle the
//! caller assembled the store from. Comparing `Arc` pointers would pass a store
//! that handed out two handles onto one empty region and fail one that reached
//! the same rows through two pools, and it is the rows that are the contract:
//! the same logical destination is the same data region.
//!
//! The clone clauses and the fresh-region clause are two halves of one
//! statement and neither stands alone. A `Clone` that built a new region would
//! pass the fresh-region clause and fail the clone clauses; a constructor that
//! shared one global region would pass the clone clauses and fail the
//! fresh-region clause.
//!
//! What the two offset clauses fix is the *configuration*, not the key
//! rendering. Both save under one scope, so a store that shared its slots would
//! fail the separation clause and one that split them would fail the sharing
//! clause whatever keys the consumers render — the key half of this is the
//! offset contract's own, fixed by the suite.
//!
//! No clause here writes `Arc::new` around a store handle. Distribution is the
//! backend's own `Clone`, carried through the erasure by
//! [`Journal`](nitinol_persistence::store::Journal) and
//! [`Album`](nitinol_persistence::store::Album), so a wiring point that had to
//! wrap a handle to name it twice would fail to compile here rather than pass
//! quietly.

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::provider::{
    CustomStore, PersistenceProvider, ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::{
    Album, EventStore, InMemoryEventStore, InMemoryOffsetStore, InMemorySnapshotStore,
    InMemoryStore, Journal, OffsetScope, OffsetStore, SnapshotStore,
};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot, TypeName,
};

/// The one event type these clauses append.
const A_FACT: EventType = EventType::new(Family::new("provider.clauses"), TypeName::new("Fact"));

fn a_fact(sequence: u64, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type: A_FACT,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

fn a_snapshot(
    aggregate_id: &AggregateId,
    sequence: u64,
    payload: &'static [u8],
) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(payload),
        created_at: Timestamp::now(),
    }
}

// Reading a backend's own handle. A backend is still an `EventStore` /
// `SnapshotStore`; what the framework hands around is the erased handle below.

/// Append one fact to `key` through the backend handle `store`.
async fn append_through(store: &dyn EventStore, key: &AggregateId, payload: &'static [u8]) {
    store
        .append(key.as_str(), vec![a_fact(1, payload)])
        .await
        .expect("appending through a reference store handle succeeds");
}

/// The payloads the backend handle `store` holds under `key`, in stored order.
async fn stream_payloads(store: &dyn EventStore, key: &AggregateId) -> Vec<Bytes> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("loading a stream of a reference store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of a reference store succeeds");
    loaded.into_iter().map(|event| event.payload).collect()
}

/// The sequence the backend handle `store` holds for `aggregate`, if any.
async fn snapshot_sequence(store: &dyn SnapshotStore, aggregate: &AggregateId) -> Option<u64> {
    store
        .load_latest(aggregate)
        .await
        .expect("loading from a reference snapshot store succeeds")
        .map(|found| found.sequence)
}

// Reading an erased handle. `Journal` and `Album` do not implement the store
// traits — the delegation is inherent — so these are separate helpers rather
// than the same ones over a `&dyn` coercion.

/// Append one fact to `key` through `journal`.
async fn append_to(journal: &Journal, key: &AggregateId, payload: &'static [u8]) {
    journal
        .append(key.as_str(), vec![a_fact(1, payload)])
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

/// The sequence `album` holds for `aggregate`, if any.
async fn album_sequence(album: &Album, aggregate: &AggregateId) -> Option<u64> {
    album
        .load_latest(aggregate)
        .await
        .expect("loading from an album succeeds")
        .map(|found| found.sequence)
}

/// The position `store` holds for `consumer`, if any.
async fn position(store: &dyn OffsetStore<Tx = ()>, consumer: &OffsetScope) -> Option<u64> {
    store
        .load(consumer)
        .await
        .expect("loading from a reference offset store succeeds")
}

/// Save `sequence` for `consumer` through `store`.
async fn save_position(store: &dyn OffsetStore<Tx = ()>, consumer: &OffsetScope, sequence: u64) {
    store
        .save(consumer, sequence, None)
        .await
        .expect("saving through a reference offset store succeeds");
}

// Distribution: a slot taken twice from one supplier

/// The journal slot. Two handles taken from one supplier reach one set of rows:
/// a record appended through the first is loaded through the second.
///
/// The type annotation is part of the clause. What a supplier distributes is the
/// erased handle, so a supplier that answered with the backend's own type — or
/// with one wrapped in an `Arc` — would fail here rather than at the wiring that
/// names one type for every store.
#[tokio::test]
async fn an_event_store_taken_twice_is_one_store() {
    // Given
    let provider = PersistenceProvider::new(InMemoryStore::default());
    let first: Journal = provider.event_store();
    let second: Journal = provider.event_store();
    let stream = AggregateId::new("provider.events.taken-twice");

    // When
    append_to(&first, &stream, b"written-through-the-first").await;

    // Then
    assert_eq!(
        payloads_of(&second, &stream).await,
        vec![Bytes::from_static(b"written-through-the-first")],
        "an event store taken twice from one supplier is one store: what one \
         handle appended is what the other loads, or the supplier has split the \
         world in two"
    );
}

/// The snapshot slot.
#[tokio::test]
async fn a_snapshot_store_taken_twice_is_one_store() {
    // Given
    let provider = PersistenceProvider::new(InMemoryStore::default());
    let first: Album = provider.snapshot_store();
    let second: Album = provider.snapshot_store();
    let aggregate = AggregateId::new("provider.snapshots.taken-twice");

    // When
    first
        .save(a_snapshot(&aggregate, 7, b"state-at-7"))
        .await
        .expect("saving through the first handle succeeds");

    // Then
    assert_eq!(
        album_sequence(&second, &aggregate).await,
        Some(7),
        "a snapshot store taken twice from one supplier is one store"
    );
}

/// The projection-offset slot.
#[tokio::test]
async fn the_projection_offset_slot_taken_twice_is_one_store() {
    // Given
    let provider = PersistenceProvider::new(InMemoryStore::default());
    let first = provider.projection_offsets();
    let second = provider.projection_offsets();
    let projection = OffsetScope::new("provider.projection.taken-twice");

    // When
    save_position(&first, &projection, 11).await;

    // Then
    assert_eq!(
        position(&second, &projection).await,
        Some(11),
        "a projection-offset slot taken twice from one supplier is one \
         store: a projector that re-resolved its handle would otherwise resume \
         from nothing and replay from the beginning"
    );
}

/// The saga-offset slot.
#[tokio::test]
async fn the_saga_offset_slot_taken_twice_is_one_store() {
    // Given
    let provider = PersistenceProvider::new(InMemoryStore::default());
    let first = provider.saga_offsets();
    let second = provider.saga_offsets();
    let consumer = OffsetScope::new("provider.delivery.taken-twice");

    // When
    save_position(&first, &consumer, 13).await;

    // Then
    assert_eq!(
        position(&second, &consumer).await,
        Some(13),
        "a saga-offset slot taken twice from one supplier is one store"
    );
}

/// A supplier's clone distributes the same slots.
///
/// The supplier is handed around by cloning rather than by an outer `Arc`, so a
/// clone that copied the store into a region of its own would have one wiring
/// point writing where no other reads.
#[tokio::test]
async fn a_clone_of_the_supplier_distributes_the_same_slots() {
    // Given
    let provider = PersistenceProvider::new(InMemoryStore::default());
    let handed_on = provider.clone();
    let stream = AggregateId::new("provider.clone.stream");
    let aggregate = AggregateId::new("provider.clone.aggregate");
    let consumer = OffsetScope::new("provider.clone.consumer");

    // When: every write goes through the original
    append_to(&provider.event_store(), &stream, b"through-the-original").await;
    provider
        .snapshot_store()
        .save(a_snapshot(&aggregate, 3, b"state-at-3"))
        .await
        .expect("saving through the original supplier succeeds");
    save_position(&provider.projection_offsets(), &consumer, 5).await;
    save_position(&provider.saga_offsets(), &consumer, 9).await;

    // Then: every read through the clone answers with it
    assert_eq!(
        payloads_of(&handed_on.event_store(), &stream).await,
        vec![Bytes::from_static(b"through-the-original")],
        "a supplier's clone distributes the journal the original distributes"
    );
    assert_eq!(
        album_sequence(&handed_on.snapshot_store(), &aggregate).await,
        Some(3),
        "a supplier's clone distributes the snapshot store the original \
         distributes"
    );
    assert_eq!(
        position(&handed_on.projection_offsets(), &consumer).await,
        Some(5),
        "a supplier's clone distributes the projection slot the original \
         distributes"
    );
    assert_eq!(
        position(&handed_on.saga_offsets(), &consumer).await,
        Some(9),
        "a supplier's clone distributes the saga slot the original \
         distributes"
    );
}

// Identity carried by the store handle itself

/// Cloning the store handle — not the supplier, and with no `Arc` around it —
/// reaches every slot's own records.
///
/// This is the identity the store itself owes: the handle a caller holds is
/// what is copied, and the data region behind it is not. A store whose `Clone`
/// built fresh state would pass every clause above, because the supplier would
/// still be holding one store.
#[tokio::test]
async fn a_clone_of_the_store_handle_reaches_every_slots_records() {
    // Given: one store, cloned before anything is written
    let store = InMemoryStore::default();
    let copy = store.clone();
    let stream = AggregateId::new("store.clone.stream");
    let aggregate = AggregateId::new("store.clone.aggregate");
    let consumer = OffsetScope::new("store.clone.consumer");

    // When: every write goes through the handles the first store answers with
    append_to(&store.events(), &stream, b"through-the-first-handle").await;
    store
        .snapshots()
        .save(a_snapshot(&aggregate, 4, b"state-at-4"))
        .await
        .expect("saving through the first store handle succeeds");
    save_position(&store.projection_offsets(), &consumer, 6).await;
    save_position(&store.saga_offsets(), &consumer, 10).await;

    // Then: every read through the clone's handles answers with it
    assert_eq!(
        payloads_of(&copy.events(), &stream).await,
        vec![Bytes::from_static(b"through-the-first-handle")],
        "a store handle's clone is the same journal: cloning copies the handle, \
         not the data region behind it"
    );
    assert_eq!(
        album_sequence(&copy.snapshots(), &aggregate).await,
        Some(4),
        "a store handle's clone is the same snapshot store"
    );
    assert_eq!(
        position(&copy.projection_offsets(), &consumer).await,
        Some(6),
        "a store handle's clone is the same projection slot"
    );
    assert_eq!(
        position(&copy.saga_offsets(), &consumer).await,
        Some(10),
        "a store handle's clone is the same saga slot"
    );
}

/// The journal store a backend supplies carries the same identity on its own,
/// away from any assembly.
///
/// A deployment that names this handle in two places — a store it assembles and
/// an upstream subscription beside it — depends on this and on nothing the
/// supplier does.
#[tokio::test]
async fn a_clone_of_an_event_store_handle_is_one_journal() {
    // Given
    let store = InMemoryEventStore::default();
    let copy = store.clone();
    let stream = AggregateId::new("event-store.clone.stream");

    // When
    append_through(&store, &stream, b"through-the-first-handle").await;

    // Then
    assert_eq!(
        stream_payloads(&copy, &stream).await,
        vec![Bytes::from_static(b"through-the-first-handle")],
        "an event store handle's clone reaches the rows the original holds"
    );
}

/// The same, for the snapshot store.
#[tokio::test]
async fn a_clone_of_a_snapshot_store_handle_is_one_store() {
    // Given
    let store = InMemorySnapshotStore::default();
    let copy = store.clone();
    let aggregate = AggregateId::new("snapshot-store.clone.aggregate");

    // When
    store
        .save(a_snapshot(&aggregate, 8, b"state-at-8"))
        .await
        .expect("saving through the first handle succeeds");

    // Then
    assert_eq!(
        snapshot_sequence(&copy, &aggregate).await,
        Some(8),
        "a snapshot store handle's clone reaches the saves the original holds"
    );
}

/// The same, for the offset store.
#[tokio::test]
async fn a_clone_of_a_offset_store_handle_is_one_store() {
    // Given
    let store = InMemoryOffsetStore::default();
    let copy = store.clone();
    let consumer = OffsetScope::new("offset-store.clone.consumer");

    // When
    save_position(&store, &consumer, 15).await;

    // Then
    assert_eq!(
        position(&copy, &consumer).await,
        Some(15),
        "an offset store handle's clone reaches the positions the original \
         holds"
    );
}

/// Constructing a store is constructing a destination, and cloning one is not.
///
/// Without this, every clone clause above would be kept by a store whose state
/// was global to the process — which would have two deployments in one test
/// binary reading each other's rows.
#[tokio::test]
async fn two_stores_constructed_apart_are_two_data_regions() {
    // Given
    let one = InMemoryStore::default();
    let another = InMemoryStore::default();
    let stream = AggregateId::new("store.default.stream");

    // When
    append_to(&one.events(), &stream, b"into-the-first-store").await;

    // Then
    assert!(
        payloads_of(&another.events(), &stream).await.is_empty(),
        "a store constructed on its own is a region of its own: only a clone of \
         a handle reaches another's rows"
    );
}

// Assembling a store from named handles

/// A store assembled from handles the caller still holds answers for each of
/// them — which is what makes a slot a *logical destination* rather than a
/// factory, and what makes mixing backends something written down.
///
/// Each handle is named as the backend's own value, and the caller keeps a clone
/// of it. That is the whole of what naming a handle takes: an assembly that
/// needed the caller to wrap one before it could be named twice would fail to
/// compile here, and one that filled a slot from anywhere but the handle it was
/// named with would fail the reads below while passing every distribution clause
/// above.
#[tokio::test]
async fn an_assembled_store_answers_for_each_handle_it_was_named_with() {
    // Given: handles the caller keeps hold of, and a store assembled over them
    let events = InMemoryEventStore::default();
    let snapshots = InMemorySnapshotStore::default();
    let offsets = InMemoryOffsetStore::default();
    let provider = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(events.clone())
            .snapshot(snapshots.clone())
            .offsets(offsets.clone())
            .build(),
    );

    let stream = AggregateId::new("custom.configured.stream");
    let aggregate = AggregateId::new("custom.configured.aggregate");
    let projection = OffsetScope::new("custom.configured.projection");
    let delivery = OffsetScope::new("custom.configured.delivery");

    // When: every write goes through the supplier's handles
    append_to(&provider.event_store(), &stream, b"through-the-supplier").await;
    provider
        .snapshot_store()
        .save(a_snapshot(&aggregate, 3, b"state-at-3"))
        .await
        .expect("saving through the supplier's handle succeeds");
    save_position(&provider.projection_offsets(), &projection, 5).await;
    save_position(&provider.saga_offsets(), &delivery, 9).await;

    // Then: every read goes to the handle the caller named
    assert_eq!(
        stream_payloads(&events, &stream).await,
        vec![Bytes::from_static(b"through-the-supplier")],
        "the journal slot answers for the handle the store was assembled from, \
         not for one of its own"
    );
    assert_eq!(
        snapshot_sequence(&snapshots, &aggregate).await,
        Some(3),
        "the snapshot slot answers for the handle the store was assembled from"
    );
    assert_eq!(
        position(&offsets, &projection).await,
        Some(5),
        "the projection-offset slot answers for the handle the store was \
         assembled from"
    );
    assert_eq!(
        position(&offsets, &delivery).await,
        Some(9),
        "the saga-offset slot answers for the handle the store was \
         assembled from"
    );
}

/// An assembled store carries the identity of the handles it was built from
/// through its own clone.
///
/// An assembly that rebuilt its slots on clone would have a deployment that
/// handed its store to two wiring points reach two worlds, even though every
/// handle it was named with was one.
#[tokio::test]
async fn a_clone_of_an_assembled_store_reaches_the_records_of_its_handles() {
    // Given
    let events = InMemoryEventStore::default();
    let store = CustomStore::builder()
        .eventstore(events.clone())
        .snapshot(InMemorySnapshotStore::default())
        .offsets(InMemoryOffsetStore::default())
        .build();
    let copy = store.clone();
    let stream = AggregateId::new("custom.clone.stream");
    let aggregate = AggregateId::new("custom.clone.aggregate");
    let consumer = OffsetScope::new("custom.clone.consumer");

    // When
    append_to(&store.events(), &stream, b"through-the-first-handle").await;
    store
        .snapshots()
        .save(a_snapshot(&aggregate, 12, b"state-at-12"))
        .await
        .expect("saving through the first store handle succeeds");
    save_position(&store.projection_offsets(), &consumer, 14).await;

    // Then
    assert_eq!(
        payloads_of(&copy.events(), &stream).await,
        vec![Bytes::from_static(b"through-the-first-handle")],
        "an assembled store's clone names the journal handle it was built from"
    );
    assert_eq!(
        album_sequence(&copy.snapshots(), &aggregate).await,
        Some(12),
        "an assembled store's clone names the snapshot handle it was built from"
    );
    assert_eq!(
        position(&copy.projection_offsets(), &consumer).await,
        Some(14),
        "an assembled store's clone names the offset handle it was built \
         from"
    );
}

// The two offset slots

/// Naming both offset slots with one handle is allowed and means one data
/// region: a position saved through one slot is answered through the other.
///
/// That is the shape a single-table backend is deployed in, so a store that
/// forced the slots apart would have no way to express it.
#[tokio::test]
async fn offset_slots_named_with_one_handle_are_one_data_region() {
    // Given
    let shared = InMemoryOffsetStore::default();
    let provider = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .offsets(shared.clone())
            .build(),
    );
    let consumer = OffsetScope::new("custom.shared-slots.consumer");

    // When
    save_position(&provider.projection_offsets(), &consumer, 21).await;

    // Then
    assert_eq!(
        position(&provider.saga_offsets(), &consumer).await,
        Some(21),
        "slots named with one handle reach one data region: sharing the \
         implementation is what a single-table deployment is, and the store has \
         to be able to express it"
    );
}

/// Naming the offset slots with handles of their own means two data
/// regions: neither slot answers for what the other holds, and initialising one
/// leaves the other's answer where it was.
///
/// This is what has a projection rebuilt without the delivery progress beside
/// it moving — the slot the projector reads is replaced, and the slot the
/// delivery host reads is not.
#[tokio::test]
async fn offset_slots_named_apart_are_separate_data_regions() {
    // Given
    let projection_slot = InMemoryOffsetStore::default();
    let delivery_slot = InMemoryOffsetStore::default();
    let provider = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .projection_offsets(projection_slot.clone())
            .saga_offsets(delivery_slot.clone())
            .build(),
    );
    let consumer = OffsetScope::new("custom.separate-slots.consumer");

    // When: only the projection slot is written
    save_position(&provider.projection_offsets(), &consumer, 31).await;

    // Then
    assert_eq!(
        position(&provider.saga_offsets(), &consumer).await,
        None,
        "a slot named with a handle of its own holds no position another slot \
         was written with; a store answering both slots from one handle would \
         have a delivery host resume from a projection's position"
    );

    // And: writing the saga slot afterwards leaves the projection slot's
    // answer where it was.
    save_position(&provider.saga_offsets(), &consumer, 37).await;

    assert_eq!(
        position(&provider.projection_offsets(), &consumer).await,
        Some(31),
        "a write to one slot does not change the other's answer, which is the \
         whole of what independent initialisation and recovery rests on"
    );
    assert_eq!(
        position(&delivery_slot, &consumer).await,
        Some(37),
        "and the delivery write landed in the handle that slot was named with"
    );
    assert_eq!(
        position(&projection_slot, &consumer).await,
        Some(31),
        "while the projection handle the caller holds still carries its own \
         position"
    );
}

// The erased handles

/// The journal a supplier answers with reaches the backend the store was
/// assembled over.
///
/// This is the erasure the framework's wiring is written against: the type the
/// backend was named with is gone from the signature, and the records are not.
/// An erasure that boxed a default of its own, or that reached a region built at
/// the erasure, would hand back a working handle onto an empty journal.
#[tokio::test]
async fn a_journal_reaches_the_backend_the_store_was_assembled_over() {
    // Given
    let events = InMemoryEventStore::default();
    let provider =
        PersistenceProvider::new(CustomStore::builder().eventstore(events.clone()).build());
    let stream = AggregateId::new("journal.assembled.stream");

    // When: the write goes through the backend handle the caller kept
    append_through(&events, &stream, b"before-the-erasure").await;

    // Then
    assert_eq!(
        payloads_of(&provider.event_store(), &stream).await,
        vec![Bytes::from_static(b"before-the-erasure")],
        "an erased journal is the store it was built from, not a copy of its \
         shape"
    );
}

/// A journal's own clone is the same journal, because that is how every wiring
/// point the framework distributes one to receives it.
///
/// The clone is the backend's: a `Journal` that duplicated the box rather than
/// asking the store to clone itself would be the two-worlds failure `PRV-1`
/// names, one level down.
#[tokio::test]
async fn a_clone_of_a_journal_is_one_journal() {
    // Given
    let journal = Journal::from(InMemoryEventStore::default());
    let copy = journal.clone();
    let stream = AggregateId::new("journal.clone.stream");

    // When
    append_to(&journal, &stream, b"through-the-first-handle").await;

    // Then
    assert_eq!(
        payloads_of(&copy, &stream).await,
        vec![Bytes::from_static(b"through-the-first-handle")],
        "a journal's clone reaches the rows the original holds"
    );
}

/// Taking a journal from a reference to one is cloning it, and there is no
/// other constructor for that.
///
/// `impl Into<Journal>` is what the framework's entry points receive, so a
/// caller holding a journal hands over `&journal` where it needs to keep its
/// own. Both forms have to land in the same place, or the choice between them
/// would silently be a choice of destination.
#[tokio::test]
async fn a_journal_taken_from_a_reference_is_the_clone_of_that_journal() {
    // Given
    let journal = Journal::from(InMemoryEventStore::default());
    let by_reference = Journal::from(&journal);
    let by_clone = journal.clone();
    let stream = AggregateId::new("journal.from-reference.stream");

    // When: the write goes through the one taken by reference
    append_to(&by_reference, &stream, b"through-the-reference").await;

    // Then
    assert_eq!(
        payloads_of(&by_clone, &stream).await,
        vec![Bytes::from_static(b"through-the-reference")],
        "`Journal::from(&journal)` and `journal.clone()` are one handle onto one \
         journal"
    );
    assert_eq!(
        payloads_of(&journal, &stream).await,
        vec![Bytes::from_static(b"through-the-reference")],
        "and the journal they were both taken from is that journal"
    );
}

/// A store holding the journal alone *is* the journal.
///
/// This is what wiring whose types are fixed before a deployment's store is
/// known names: a saga's dispatch mapping names one system type when the mapping
/// is written. The handle it names is a `Journal`, and handing one to a supplier
/// has to reach the records it holds.
#[tokio::test]
async fn a_journal_handed_to_a_supplier_is_the_store_it_distributes() {
    // Given
    let journal = Journal::from(InMemoryEventStore::default());
    let provider = PersistenceProvider::new(journal.clone());
    let stream = AggregateId::new("journal.as-store.stream");

    // When
    append_to(&journal, &stream, b"through-the-journal").await;

    // Then
    assert_eq!(
        payloads_of(&provider.event_store(), &stream).await,
        vec![Bytes::from_static(b"through-the-journal")],
        "a supplier over a journal distributes that journal, which is what makes \
         the journal-only store the journal itself rather than a wrapper around \
         one"
    );
}

/// The album a supplier answers with reaches the snapshot backend the store was
/// assembled over, and its clone is that album.
///
/// The snapshot half of the erasure, with the same two failures to exclude: an
/// album that boxed a store of its own, and a clone that did.
#[tokio::test]
async fn an_album_and_its_clone_reach_the_snapshot_backend_they_were_built_over() {
    // Given
    let snapshots = InMemorySnapshotStore::default();
    let provider = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .snapshot(snapshots.clone())
            .build(),
    );
    let aggregate = AggregateId::new("album.assembled.aggregate");
    let album: Album = provider.snapshot_store();
    let copy = album.clone();

    // When: the save goes through the backend handle the caller kept
    snapshots
        .save(a_snapshot(&aggregate, 17, b"state-at-17"))
        .await
        .expect("saving through the named snapshot handle succeeds");

    // Then
    assert_eq!(
        album_sequence(&album, &aggregate).await,
        Some(17),
        "an erased album is the snapshot store it was built from"
    );
    assert_eq!(
        album_sequence(&copy, &aggregate).await,
        Some(17),
        "and its clone is that same store"
    );
}

/// Taking an album from a reference to one is cloning it, as with the journal.
#[tokio::test]
async fn an_album_taken_from_a_reference_is_the_clone_of_that_album() {
    // Given
    let album = Album::from(InMemorySnapshotStore::default());
    let by_reference = Album::from(&album);
    let by_clone = album.clone();
    let aggregate = AggregateId::new("album.from-reference.aggregate");

    // When
    by_reference
        .save(a_snapshot(&aggregate, 19, b"state-at-19"))
        .await
        .expect("saving through the album taken by reference succeeds");

    // Then
    assert_eq!(
        album_sequence(&by_clone, &aggregate).await,
        Some(19),
        "`Album::from(&album)` and `album.clone()` are one handle onto one \
         snapshot store"
    );
}
