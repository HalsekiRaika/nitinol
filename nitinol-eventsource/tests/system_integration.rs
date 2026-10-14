// Integration tests for EventSourceSystem.
//
// Tests verify:
// - Builder pattern: EventSourceSystem::builder(ps).with_persistence(..).build()
// - spawn_aggregate creates a working AggregateProxy
// - A second spawn_aggregate with the same id addresses the aggregate the first
//   call resolved, rather than a second activation of it
// - ProjectorProps wired from the system projects catch-up events through the
//   codec each event type names
// - Snapshot save → load → state restoration via EventSourceSystem
//
// The system itself carries no codec: an event names the one that writes it
// down, so there is nothing to hand the builder and nothing to ask it for.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::SnapshotPersistor;
use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Decider, Decision, Event,
    ProjectionContext, ProjectionId, Projector, ProjectorProps, Query, Snapshotable,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, InMemorySnapshotStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new(""), TypeName::new("SysIntIncremented"));

    type Codec = JsonCodec;
}

// Fixtures: aggregate (non-snapshotable)

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: aggregate (snapshotable, snapshot = u64)

#[derive(Default)]
struct CounterWithSnapshot {
    value: u64,
}

impl Aggregate for CounterWithSnapshot {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for CounterWithSnapshot {
    type Snapshot = u64;

    type Codec = JsonCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// Fixtures: commands and queries

struct Increment;
struct GetCount;

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

impl Decider<Increment> for CounterWithSnapshot {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for CounterWithSnapshot {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixtures: JsonCodec — generic serde_json-backed Codec

/// Unit-struct codec backed by serde_json.
/// Implements Codec<E> for any E that is Serialize + DeserializeOwned.
///
/// Never constructed: it is named by `Incremented` and `CounterWithSnapshot`
/// at the type level, and every call goes through its associated functions.
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

// Fixtures: Projector for Incremented events

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

async fn wait_for_count(counter: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} calls"));
}

// Test: EventSourceSystem builder creates a system

/// EventSourceSystem::builder(ps).with_persistence(..).build() compiles and
/// produces an EventSourceSystem that exposes process_system().
#[tokio::test]
async fn event_source_system_builder_produces_system() {
    // Given
    let ps = ProcessSystem::new().await;

    // When
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .build(),
        ))
        .build();

    // Then: process_system() is accessible (type-level proof)
    let _ = system.process_system();
}

// Test: spawn_aggregate creates a working proxy

/// spawn_aggregate creates an AggregateProxy that accepts commands and writes
/// the facts they produce to the store it was spawned on.
#[tokio::test]
async fn spawn_aggregate_creates_working_aggregate_proxy() {
    // Given
    let ps = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .build(),
    );
    let store = persistence.event_store();
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence)
        .build();
    let id = AggregateId::new("sys-spawn-basic");

    // When
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;

    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(&id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");
    let event_types: Vec<EventType> = loaded.into_iter().map(|event| event.event_type).collect();
    assert_eq!(
        event_types,
        vec![Incremented::EVENT_TYPE],
        "the accepted command's fact must have reached the stream"
    );
}

// Test: second spawn_aggregate addresses the same aggregate

/// Resolving the same id twice yields two references to one aggregate.
///
/// The third dispatch is the discriminator: two activations would each carry
/// their own sequence counter, so `proxy1` would still be at sequence 1 after
/// `proxy2` wrote sequence 2, and the store would reject its append.
#[tokio::test]
async fn spawn_aggregate_second_call_addresses_the_same_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .build(),
    );
    let store = persistence.event_store();
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence)
        .build();
    let id = AggregateId::new("sys-replay-state");

    let proxy1 = system.spawn_aggregate::<Counter>(id.clone()).await;
    proxy1.ask(Increment).await.expect("ask must succeed");

    // When: resolve the same id again, then dispatch through both references
    let proxy2 = system.spawn_aggregate::<Counter>(id.clone()).await;
    proxy2
        .ask(Increment)
        .await
        .expect("the second reference must dispatch to the same aggregate");
    proxy1
        .ask(Increment)
        .await
        .expect("the first reference must still be writing the same stream");

    // Then
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 3,
        "one activation must have applied all three increments"
    );

    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(&id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");
    let sequences: Vec<u64> = loaded.iter().map(|event| event.sequence).collect();
    assert_eq!(
        sequences,
        vec![1, 2, 3],
        "a single writer must number the appends 1, 2, 3 without a collision"
    );
}

// Test: sequential ask calls accumulate state

/// Three sequential ask(Increment) calls advance the counter to 3.
#[tokio::test]
async fn spawn_aggregate_sequential_increments_accumulate_state() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .build(),
        ))
        .build();
    let proxy = system
        .spawn_aggregate::<Counter>(AggregateId::new("sys-seq"))
        .await;

    // When
    proxy.ask(Increment).await.expect("ask 1");
    proxy.ask(Increment).await.expect("ask 2");
    proxy.ask(Increment).await.expect("ask 3");

    let count = proxy.exec(GetCount).await.expect("exec");

    // Then
    assert_eq!(count, 3, "count must be 3 after three Increment commands");
}

// Test: a projector wired from the system decodes through the named codec

/// A `ProjectorProps` built against the system's process runtime projects the
/// catch-up events it is asked for, decoding them through the codec
/// `Incremented` names — `with_event::<E>()` is handed nothing.
#[tokio::test]
async fn projector_props_decode_through_the_codec_each_event_names() {
    // Given
    let ps = ProcessSystem::new().await;
    let offset_store = InMemoryOffsetStore::default();
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let event_store = persistence.event_store();
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence.clone())
        .build();

    let agg_id = AggregateId::new("sys-proj-agg");

    // Pre-append one event so catch-up has something to project
    event_store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: EventType::new(Family::new(""), TypeName::new("SysIntIncremented")),
                payload: serde_json::to_vec(&Incremented).map(Bytes::from).expect(
                    "Incremented is a plain Serialize unit struct, so JSON encoding cannot fail",
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: wire a projector for the event type, naming no codec
    let _proxy = ProjectorProps::new(ProjectionId::new("sys-proj"), &persistence, move || {
        CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(agg_id)
    .spawn(system.process_system())
    .await;

    // Then: project() is called for the one stored event
    wait_for_count(&count, &notify, 1).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project(Incremented) must be called once for the stored catch-up event"
    );
}

// Test: aggregate_props allows manual snapshot configuration

/// system.aggregate_props() returns a pre-wired AggregateProps.
/// Passing it the snapshot persistor allows snapshotable aggregates to restore
/// state from a saved snapshot, read back through the codec
/// `CounterWithSnapshot` names.
#[tokio::test]
async fn snapshot_roundtrip_via_event_source_system() {
    // Given
    let ps = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .snapshot(InMemorySnapshotStore::default())
            .build(),
    );
    let system = EventSourceSystem::builder(ps)
        .with_persistence(persistence.clone())
        .build();

    let snapshot_ref = SnapshotPersistor::spawn(system.process_system(), &persistence).await;
    let id = AggregateId::new("sys-snap-restore");

    // Manually save a JSON-encoded snapshot (value=7 at sequence=7)
    let snapshot_payload = serde_json::to_vec(&7u64)
        .map(Bytes::from)
        .expect("snapshot serialisation must succeed");
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 7,
            payload: snapshot_payload,
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When: spawn a process that restores from the snapshot through the codec
    // the aggregate names
    let proxy = system
        .aggregate_props::<CounterWithSnapshot>(id)
        .with_snapshot_persistor(snapshot_ref)
        .spawn()
        .await;

    let count = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then: state is restored from snapshot (value=7)
    assert_eq!(
        count, 7,
        "snapshot restore must produce the saved value (7)"
    );
}
