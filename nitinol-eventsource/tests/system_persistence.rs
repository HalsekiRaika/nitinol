//! The persistence provider an `EventSourceSystem` is built with, and the
//! wiring that resolves its slots.
//!
//! ```gherkin
//!   Scenario: aggregates land in the provider's event store
//!     Given a system built with a persistence provider
//!     When an aggregate is spawned and told to decide
//!     Then its events are in the store the provider was configured with
//!
//!   Scenario: the snapshot persistor resolves the snapshot slot
//!     Given a system built with a provider that supplies a snapshot store
//!     When a snapshot is saved through the persistor the system hands back
//!     Then it is in the store the provider was configured with
//!
//!   Scenario: the projector resolves two slots at once
//!     Given a system built with a provider that supplies projection checkpoints
//!     When a projector wired from the system catches up
//!     Then it read the provider's event store
//!     And it recorded its position in the provider's projection slot
//! ```
//!
//! Every clause reads the store the provider was *configured with* rather than
//! the one a handle happens to answer with, because an implementation that
//! reaches the right process and the wrong store is the failure these exist to
//! catch: a `spawn_aggregate` that built an event store of its own per call
//! would hand back a working proxy and leave nothing behind.
//!
//! A slot a provider does not supply is refused by the compiler rather than
//! here, so the type-level half of this is fixed by the `compile_fail` doctests
//! on the entry points in `src/` — an integration test cannot express a call
//! that must not compile.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Decider, Decision, Event,
    ProjectionContext, Projector, ProjectorProps, SnapshotPersistor,
};
use nitinol_persistence::provider::{Persistence, PersistenceProvider};
use nitinol_persistence::store::{
    CheckpointStore, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
    InMemorySnapshotStore, SnapshotStore,
};
use nitinol_persistence::{
    AggregateId, EventType, Family, LoadQuery, PersistedSnapshot, ProjectionId, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures

struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("system.persistence"),
        TypeName::new("Incremented"),
    );

    type Codec = JsonCodec;
}

#[derive(Default)]
struct Counter;

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {}
}

struct Increment;

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

struct CountingProjector {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Incremented> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// Helpers

/// How long a clause waits for work a process performs on its own loop.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Event types recorded under `key` in `store`, in stored order.
///
/// Reading the stream back out of a store the clause *named itself* is the only
/// way to tell which store a spawn resolved: the proxy looks identical either
/// way.
async fn stream_event_types(store: &Arc<dyn EventStore>, key: &AggregateId) -> Vec<EventType> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded.into_iter().map(|event| event.event_type).collect()
}

fn a_snapshot(aggregate_id: &AggregateId, sequence: u64) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(b"state"),
        created_at: jiff::Timestamp::now(),
    }
}

async fn wait_for_count(counter: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        loop {
            let notified = notify.notified();
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} project() calls"));
}

/// Wait until `store` holds a position for `projection`, then answer it.
async fn wait_for_checkpoint(
    store: &Arc<InMemoryCheckpointStore>,
    projection: &ProjectionId,
    label: &str,
) -> u64 {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        loop {
            if let Some(held) = store
                .load(projection)
                .await
                .expect("loading a checkpoint of an in-memory store succeeds")
            {
                return held;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}: no position was ever recorded"))
}

// The event-store slot, through every aggregate entry point

/// `spawn_aggregate(id)` — the only form there is — must persist onto the event
/// store the provider supplies.
#[tokio::test]
async fn spawn_aggregate_persists_onto_the_providers_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(Arc::clone(&events)))
        .build();
    let id = AggregateId::new("system-persistence-spawn-aggregate");

    // When
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&events, &id).await,
        vec![Incremented::EVENT_TYPE],
        "a spawn resolves the provider's event store, so that is where the \
         aggregate's events are"
    );
}

/// `aggregate_props(id)` must pre-wire the same slot, so a caller that needs
/// further configuration before spawning does not have to re-supply a store.
#[tokio::test]
async fn aggregate_props_persists_onto_the_providers_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(Arc::clone(&events)))
        .build();
    let id = AggregateId::new("system-persistence-aggregate-props");

    // When
    let proxy = system
        .aggregate_props::<Counter>(id.clone())
        .spawn(system.process_system())
        .await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&events, &id).await,
        vec![Incremented::EVENT_TYPE],
        "props taken from the system carry the provider's event store"
    );
}

/// `aggregate_proxy(id)` builds a reference without awaiting, and must resolve
/// the same slot when it is first dispatched to.
#[tokio::test]
async fn aggregate_proxy_persists_onto_the_providers_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(Arc::clone(&events)))
        .build();
    let id = AggregateId::new("system-persistence-aggregate-proxy");

    // When
    let proxy = system.aggregate_proxy::<Counter>(id.clone());
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&events, &id).await,
        vec![Incremented::EVENT_TYPE],
        "a reference resolved at dispatch time activates onto the provider's \
         event store"
    );
}

/// The provider the system carries is reachable from it, and it is the one the
/// spawns resolve — which is what lets wiring outside this crate (a saga's own
/// journal, an upstream subscription) resolve the same slot rather than keep an
/// `Arc` of its own beside the system.
#[tokio::test]
async fn the_provider_the_system_carries_is_the_one_its_spawns_resolve() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(Arc::clone(&events)))
        .build();
    let id = AggregateId::new("system-persistence-accessor");

    // When
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    let from_the_system = system.persistence().event_store();
    assert_eq!(
        stream_event_types(&from_the_system, &id).await,
        vec![Incremented::EVENT_TYPE],
        "the store the accessor answers with is the store the spawn wrote to"
    );
}

// The snapshot slot

/// The persistor the system hands back must write to the snapshot store the
/// provider supplies.
#[tokio::test]
async fn the_systems_snapshot_persistor_writes_to_the_providers_snapshot_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(InMemorySnapshotStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(events).with_snapshot_store(Arc::clone(&snapshots)))
        .build();
    let id = AggregateId::new("system-persistence-snapshot");

    // When
    let persistor = system.snapshot_persistor().await;
    persistor
        .save(a_snapshot(&id, 4))
        .await
        .expect("saving through the persistor must succeed");

    // Then
    assert_eq!(
        snapshots
            .load_latest(&id)
            .await
            .expect("loading from the configured snapshot store succeeds")
            .map(|found| found.sequence),
        Some(4),
        "the persistor the system distributes resolves the provider's snapshot \
         slot, so the save is in the store the provider was configured with"
    );
}

/// The persistor the system distributes and one spawned from the provider
/// directly reach one store: a snapshot saved through either is loaded through
/// the other.
///
/// The low-level entry point stays because a lifecycle pinned to one aggregate
/// is started without a system; what must not follow is a second world.
#[tokio::test]
async fn a_persistor_spawned_from_the_provider_reaches_the_same_snapshot_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(InMemorySnapshotStore::default());
    let persistence = Persistence::new(events).with_snapshot_store(Arc::clone(&snapshots));
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence.clone())
        .build();
    let id = AggregateId::new("system-persistence-snapshot-both-ways");

    // When: the save goes through the persistor spawned from the provider
    let directly = SnapshotPersistor::spawn(system.process_system(), &persistence).await;
    directly
        .save(a_snapshot(&id, 6))
        .await
        .expect("saving through the directly spawned persistor must succeed");

    // Then: the load through the system's persistor answers with it
    assert_eq!(
        system
            .snapshot_persistor()
            .await
            .load_latest(id)
            .await
            .expect("loading through the system's persistor must succeed")
            .map(|found| found.sequence),
        Some(6),
        "both wiring entry points resolve the provider's snapshot slot, so \
         neither has a store of its own"
    );
}

// The projection-checkpoint slot

/// The projector the system wires must follow the provider's event store and
/// record its position in the provider's projection slot.
#[tokio::test]
async fn the_systems_projector_follows_the_providers_event_store_and_projection_slot() {
    // Given: a system over a provider supplying events and projection
    // checkpoints, with an aggregate that has already decided twice
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let projection_checkpoints = Arc::new(InMemoryCheckpointStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(
            Persistence::new(Arc::clone(&events))
                .with_projection_checkpoints(Arc::clone(&projection_checkpoints)),
        )
        .build();
    let id = AggregateId::new("system-persistence-projection");
    let projection = ProjectionId::new("system-persistence-projection-read-model");

    let aggregate = system.spawn_aggregate::<Counter>(id.clone()).await;
    aggregate.ask(Increment).await.expect("ask must succeed");
    aggregate.ask(Increment).await.expect("ask must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_for_producer = Arc::clone(&count);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _projector = system
        .projector_props(projection.clone(), move || CountingProjector {
            count: Arc::clone(&count_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_event::<Incremented>()
        .catchup_from_aggregate(id)
        .spawn(system.process_system())
        .await;

    // Then: it read the provider's event store
    wait_for_count(&count, &notify, 2).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the projector catches up over the provider's event store, so it sees \
         both events the aggregate persisted there"
    );

    // And: it recorded into the provider's projection slot
    let held = wait_for_checkpoint(
        &projection_checkpoints,
        &projection,
        "the projector must record into the provider's projection slot",
    )
    .await;
    assert!(
        held >= 2,
        "the position recorded in the provider's projection slot must have \
         reached the second event, and is {held}"
    );
}

/// `ProjectorProps::new` takes the provider and resolves both slots from it, so
/// a projector built without going through the system reaches the same two
/// stores.
#[tokio::test]
async fn projector_props_resolves_both_slots_from_the_provider() {
    // Given
    let ps = ProcessSystem::new().await;
    let events: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let projection_checkpoints = Arc::new(InMemoryCheckpointStore::default());
    let persistence = Persistence::new(Arc::clone(&events))
        .with_projection_checkpoints(Arc::clone(&projection_checkpoints));
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence.clone())
        .build();
    let id = AggregateId::new("system-persistence-projector-props");
    let projection = ProjectionId::new("system-persistence-projector-props-read-model");

    let aggregate = system.spawn_aggregate::<Counter>(id.clone()).await;
    aggregate.ask(Increment).await.expect("ask must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_for_producer = Arc::clone(&count);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _projector = ProjectorProps::new(projection.clone(), &persistence, move || {
        CountingProjector {
            count: Arc::clone(&count_for_producer),
            notify: Arc::clone(&notify_for_producer),
        }
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(id)
    .spawn(system.process_system())
    .await;

    // Then
    wait_for_count(&count, &notify, 1).await;
    let held = wait_for_checkpoint(
        &projection_checkpoints,
        &projection,
        "props built from the provider must record into its projection slot",
    )
    .await;
    assert!(
        held >= 1,
        "the position recorded in the provider's projection slot must have \
         reached the event, and is {held}"
    );
}
