//! What a persistence provider owes about the stores it hands out.
//!
//! ```gherkin
//!   Scenario: distribution is idempotent
//!     Given an EventStore taken twice from one provider
//!     When an event appended through one handle is loaded through the other
//!     Then the two are observed as one store
//!
//!   Scenario: a handle answers for the store it was configured with
//!     Given a provider assembled from stores the caller still holds
//!     When a record is written through the provider's handle
//!     Then the store the caller holds is where it landed
//!
//!   Scenario: two checkpoint slots over one store are one data region
//!     Given a provider whose checkpoint slots were configured with one store
//!     When a position is saved through the projection handle
//!     Then the delivery handle answers with it
//!
//!   Scenario: two checkpoint slots configured apart are two data regions
//!     Given a provider whose checkpoint slots were configured with two stores
//!     When a position is saved through the projection handle
//!     Then the delivery handle holds no position
//!     And a later save through the delivery handle leaves the projection
//!         handle's answer where it was
//! ```
//!
//! Every clause here reads back through a *second* handle, or through the store
//! the caller configured the provider with. Comparing `Arc` pointers would pass
//! a provider that handed out two handles onto one empty store and fail one that
//! reached the same rows through two pools, and it is the rows that are the
//! contract: the same logical destination is the same data region.
//!
//! What the two checkpoint clauses fix is the *configuration*, not the key
//! rendering. Both save under one scope, so a store that shared its slots would
//! fail the separation clause and one that split them would fail the sharing
//! clause whatever keys the consumers render — the key half of this is the
//! checkpoint contract's own, fixed by the suite.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::provider::{
    DeliveryCheckpointProvider, Persistence, PersistenceProvider, ProjectionCheckpointProvider,
    SnapshotStoreProvider,
};
use nitinol_persistence::store::{
    CheckpointStore, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
    InMemorySnapshotStore, SnapshotStore,
};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot, ProjectionId,
    TypeName,
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

/// The payloads `store` holds under `key`, in stored order.
async fn stream_payloads(store: &Arc<dyn EventStore>, key: &AggregateId) -> Vec<Bytes> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("loading a stream of a reference store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of a reference store succeeds");
    loaded.into_iter().map(|event| event.payload).collect()
}

/// PRV-1, the event-store slot. Two handles taken from one provider reach one
/// set of rows: a record appended through the first is loaded through the
/// second.
#[tokio::test]
async fn an_event_store_taken_twice_is_one_store() {
    // Given
    let persistence = Persistence::in_memory();
    let first = persistence.event_store();
    let second = persistence.event_store();
    let stream = AggregateId::new("provider.events.taken-twice");

    // When
    first
        .append(
            stream.as_str(),
            vec![a_fact(1, b"written-through-the-first")],
        )
        .await
        .expect("appending through the first handle succeeds");

    // Then
    assert_eq!(
        stream_payloads(&second, &stream).await,
        vec![Bytes::from_static(b"written-through-the-first")],
        "an event store taken twice from one provider is one store: what one \
         handle appended is what the other loads, or the provider has split the \
         world in two"
    );
}

/// PRV-1, the snapshot slot.
#[tokio::test]
async fn a_snapshot_store_taken_twice_is_one_store() {
    // Given
    let persistence = Persistence::in_memory();
    let first = persistence.snapshot_store();
    let second = persistence.snapshot_store();
    let aggregate = AggregateId::new("provider.snapshots.taken-twice");

    // When
    first
        .save(a_snapshot(&aggregate, 7, b"state-at-7"))
        .await
        .expect("saving through the first handle succeeds");

    // Then
    let found = second
        .load_latest(&aggregate)
        .await
        .expect("loading through the second handle succeeds")
        .expect("a snapshot saved through one handle is present through the other");
    assert_eq!(
        found.sequence, 7,
        "a snapshot store taken twice from one provider is one store"
    );
    assert_eq!(
        found.payload,
        Bytes::from_static(b"state-at-7"),
        "the state saved through one handle is the state the other reads back"
    );
}

/// PRV-1, the projection-checkpoint slot.
#[tokio::test]
async fn the_projection_checkpoint_slot_taken_twice_is_one_store() {
    // Given
    let persistence = Persistence::in_memory();
    let first = persistence.projection_checkpoints();
    let second = persistence.projection_checkpoints();
    let projection = ProjectionId::new("provider.projection.taken-twice");

    // When
    first
        .save(&projection, 11, None)
        .await
        .expect("saving through the first handle succeeds");

    // Then
    assert_eq!(
        second
            .load(&projection)
            .await
            .expect("loading through the second handle succeeds"),
        Some(11),
        "a projection-checkpoint slot taken twice from one provider is one \
         store: a projector that re-resolved its handle would otherwise resume \
         from nothing and replay from the beginning"
    );
}

/// PRV-1, the delivery-checkpoint slot.
#[tokio::test]
async fn the_delivery_checkpoint_slot_taken_twice_is_one_store() {
    // Given
    let persistence = Persistence::in_memory();
    let first = persistence.delivery_checkpoints();
    let second = persistence.delivery_checkpoints();
    let consumer = ProjectionId::new("provider.delivery.taken-twice");

    // When
    first
        .save(&consumer, 13, None)
        .await
        .expect("saving through the first handle succeeds");

    // Then
    assert_eq!(
        second
            .load(&consumer)
            .await
            .expect("loading through the second handle succeeds"),
        Some(13),
        "a delivery-checkpoint slot taken twice from one provider is one store"
    );
}

/// PRV-2. A handle answers for the store the provider was configured with —
/// what makes the slot a *logical destination* rather than a factory. A provider
/// building a store of its own per slot would pass every clause above and fail
/// here.
#[tokio::test]
async fn every_slot_answers_for_the_store_it_was_configured_with() {
    // Given: stores the caller keeps hold of, and a provider over them
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(InMemorySnapshotStore::default());
    let checkpoints = Arc::new(InMemoryCheckpointStore::default());
    let persistence = Persistence::new(Arc::clone(&events))
        .with_snapshot_store(Arc::clone(&snapshots))
        .with_checkpoints(Arc::clone(&checkpoints));

    let stream = AggregateId::new("provider.configured.stream");
    let aggregate = AggregateId::new("provider.configured.aggregate");
    let projection = ProjectionId::new("provider.configured.projection");
    let delivery = ProjectionId::new("provider.configured.delivery");

    // When: every write goes through the provider's handles
    persistence
        .event_store()
        .append(stream.as_str(), vec![a_fact(1, b"through-the-provider")])
        .await
        .expect("appending through the provider's handle succeeds");
    persistence
        .snapshot_store()
        .save(a_snapshot(&aggregate, 3, b"state-at-3"))
        .await
        .expect("saving through the provider's handle succeeds");
    persistence
        .projection_checkpoints()
        .save(&projection, 5, None)
        .await
        .expect("saving a projection position through the provider's handle succeeds");
    persistence
        .delivery_checkpoints()
        .save(&delivery, 9, None)
        .await
        .expect("saving a delivery position through the provider's handle succeeds");

    // Then: every read goes to the store the caller configured
    assert_eq!(
        stream_payloads(&events, &stream).await,
        vec![Bytes::from_static(b"through-the-provider")],
        "the event-store slot answers for the store the provider was \
         configured with, not for one of its own"
    );
    assert_eq!(
        snapshots
            .load_latest(&aggregate)
            .await
            .expect("loading from the configured snapshot store succeeds")
            .map(|found| found.sequence),
        Some(3),
        "the snapshot slot answers for the store the provider was configured \
         with"
    );
    assert_eq!(
        checkpoints
            .load(&projection)
            .await
            .expect("loading from the configured checkpoint store succeeds"),
        Some(5),
        "the projection-checkpoint slot answers for the store the provider was \
         configured with"
    );
    assert_eq!(
        checkpoints
            .load(&delivery)
            .await
            .expect("loading from the configured checkpoint store succeeds"),
        Some(9),
        "the delivery-checkpoint slot answers for the store the provider was \
         configured with"
    );
}

/// PRV-4, the sharing half. Configuring both checkpoint slots with one store is
/// allowed and means one data region: a position saved through one slot is
/// answered through the other.
///
/// That is the shape a single-table backend is deployed in, so a provider that
/// forced the slots apart would have no way to express it.
#[tokio::test]
async fn checkpoint_slots_configured_with_one_store_are_one_data_region() {
    // Given
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let shared = Arc::new(InMemoryCheckpointStore::default());
    let persistence = Persistence::new(events).with_checkpoints(Arc::clone(&shared));
    let consumer = ProjectionId::new("provider.shared-slots.consumer");

    // When
    persistence
        .projection_checkpoints()
        .save(&consumer, 21, None)
        .await
        .expect("saving through the projection slot succeeds");

    // Then
    assert_eq!(
        persistence
            .delivery_checkpoints()
            .load(&consumer)
            .await
            .expect("loading through the delivery slot succeeds"),
        Some(21),
        "slots configured with one store reach one data region: sharing the \
         implementation is what a single-table deployment is, and the provider \
         has to be able to express it"
    );
}

/// PRV-4, the separation half. Configuring the checkpoint slots with stores of
/// their own means two data regions: neither slot answers for what the other
/// holds, and initialising one leaves the other's answer where it was.
///
/// This is what has a projection rebuilt without the delivery progress beside
/// it moving — the slot the projector reads is replaced, and the slot the
/// delivery host reads is not.
#[tokio::test]
async fn checkpoint_slots_configured_apart_are_separate_data_regions() {
    // Given
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let projection_slot = Arc::new(InMemoryCheckpointStore::default());
    let delivery_slot = Arc::new(InMemoryCheckpointStore::default());
    let persistence = Persistence::new(events)
        .with_projection_checkpoints(Arc::clone(&projection_slot))
        .with_delivery_checkpoints(Arc::clone(&delivery_slot));
    let consumer = ProjectionId::new("provider.separate-slots.consumer");

    // When: only the projection slot is written
    persistence
        .projection_checkpoints()
        .save(&consumer, 31, None)
        .await
        .expect("saving through the projection slot succeeds");

    // Then
    assert_eq!(
        persistence
            .delivery_checkpoints()
            .load(&consumer)
            .await
            .expect("loading through the delivery slot succeeds"),
        None,
        "a slot configured with a store of its own holds no position another \
         slot was written with; a provider answering both slots from one store \
         would have a delivery host resume from a projection's position"
    );

    // And: writing the delivery slot afterwards leaves the projection slot's
    // answer where it was.
    persistence
        .delivery_checkpoints()
        .save(&consumer, 37, None)
        .await
        .expect("saving through the delivery slot succeeds");

    assert_eq!(
        persistence
            .projection_checkpoints()
            .load(&consumer)
            .await
            .expect("loading through the projection slot succeeds"),
        Some(31),
        "a write to one slot does not change the other's answer, which is the \
         whole of what independent initialisation and recovery rests on"
    );
    assert_eq!(
        delivery_slot
            .load(&consumer)
            .await
            .expect("loading from the configured delivery store succeeds"),
        Some(37),
        "and the delivery write landed in the store that slot was configured \
         with"
    );
    assert_eq!(
        projection_slot
            .load(&consumer)
            .await
            .expect("loading from the configured projection store succeeds"),
        Some(31),
        "while the projection store the caller holds still carries its own \
         position"
    );
}
