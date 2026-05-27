// Integration tests for EventSourceSystem.
//
// Tests verify:
// - Builder pattern: EventSourceSystem::new(ps).with_codec::<C>().build()
// - spawn_aggregate creates a working AggregateProxy
// - A second spawn_aggregate with the same id replays state from the first
// - system.codec::<E>() returns a working Arc<dyn ErasedCodec<E>>
// - system.codec() can be passed directly to ProjectorProps::with_event()
// - Snapshot save → load → state restoration via EventSourceSystem

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::SnapshotPersistor;
use nitinol_eventsource::{
    codec::Codec, codec::ErasedCodec, system::EventSourceSystem, Aggregate, Context, Decider,
    Effect, Event, ProjectionContext, Projector, ProjectorProps, Receive as EvtReceive,
    Snapshotable,
};
use nitinol_persistence::store::{
    EventStore, InMemoryCheckpointStore, InMemoryEventStore, InMemorySnapshotStore,
};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, PersistedSnapshot, ProjectionId,
};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("SysIntIncremented");
}

// ---------------------------------------------------------------------------
// Fixtures: aggregate (non-snapshotable)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Fixtures: aggregate (snapshotable, snapshot = u64)
// ---------------------------------------------------------------------------

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

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// ---------------------------------------------------------------------------
// Fixtures: commands and queries
// ---------------------------------------------------------------------------

struct Increment;
struct GetCount;

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[async_trait]
impl EvtReceive<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

#[async_trait]
impl Decider<Increment> for CounterWithSnapshot {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[async_trait]
impl EvtReceive<GetCount> for CounterWithSnapshot {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: JsonCodec — generic serde_json-backed Codec
// ---------------------------------------------------------------------------

/// Unit-struct codec backed by serde_json.
/// Implements Codec<E> for any E that is Serialize + DeserializeOwned.
///
/// Requires Default (so EventSourceSystem can create instances via CodecRegistry).
#[derive(Default)]
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

// ---------------------------------------------------------------------------
// Fixtures: Projector for Incremented events
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test: EventSourceSystem builder creates a system
// ---------------------------------------------------------------------------

/// EventSourceSystem::new(ps).with_codec::<C>().build() compiles and produces
/// an EventSourceSystem that exposes process_system().
#[tokio::test]
async fn event_source_system_builder_produces_system() {
    // Given
    let ps = ProcessSystem::new().await;

    // When
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // Then: process_system() is accessible (type-level proof)
    let _ = system.process_system();
}

// ---------------------------------------------------------------------------
// Test: spawn_aggregate creates a working proxy
// ---------------------------------------------------------------------------

/// spawn_aggregate creates an AggregateProxy that accepts commands.
#[tokio::test]
async fn spawn_aggregate_creates_working_aggregate_proxy() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("sys-spawn-basic");

    // When
    let proxy = system.spawn_aggregate::<Counter>(id, store).await;

    let events = proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        events,
        vec![Incremented],
        "ask must return the persisted event"
    );
}

// ---------------------------------------------------------------------------
// Test: second spawn_aggregate replays state
// ---------------------------------------------------------------------------

/// After ask(Increment) on process 1, spawning a second process with the same id
/// and the same `Arc<dyn EventStore>` replays the stored event and restores count to 1.
#[tokio::test]
async fn spawn_aggregate_second_process_replays_state() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("sys-replay-state");

    // Write one event via process 1
    {
        let proxy = system
            .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&store))
            .await;
        proxy.ask(Increment).await.expect("ask must succeed");
    }

    // When: spawn a fresh process for the same id — triggers replay in on_start
    let proxy2 = system.spawn_aggregate::<Counter>(id, store).await;

    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: replayed state equals 1
    assert_eq!(
        count, 1,
        "replay must restore count to 1 after one persisted Increment"
    );
}

// ---------------------------------------------------------------------------
// Test: sequential ask calls accumulate state
// ---------------------------------------------------------------------------

/// Three sequential ask(Increment) calls advance the counter to 3.
#[tokio::test]
async fn spawn_aggregate_sequential_increments_accumulate_state() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = system
        .spawn_aggregate::<Counter>(AggregateId::new("sys-seq"), store)
        .await;

    // When
    proxy.ask(Increment).await.expect("ask 1");
    proxy.ask(Increment).await.expect("ask 2");
    proxy.ask(Increment).await.expect("ask 3");

    let count = proxy.exec(GetCount).await.expect("exec");

    // Then
    assert_eq!(count, 3, "count must be 3 after three Increment commands");
}

// ---------------------------------------------------------------------------
// Test: system.codec() returns a usable Arc<dyn ErasedCodec<E>>
// ---------------------------------------------------------------------------

/// system.codec::<E>() returns a codec that can encode and decode E.
#[tokio::test]
async fn system_codec_returns_usable_erased_codec() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // When
    let codec: Arc<dyn ErasedCodec<Incremented>> = system.codec::<Incremented>();
    let encoded = codec.encode(&Incremented).expect("encode must succeed");
    let decoded: Incremented = codec.decode(&encoded).expect("decode must succeed");

    // Then
    assert_eq!(
        decoded, Incremented,
        "codec roundtrip must preserve the event"
    );
}

// ---------------------------------------------------------------------------
// Test: system.codec() integrates with ProjectorProps::with_event
// ---------------------------------------------------------------------------

/// system.codec::<E>() can be passed to ProjectorProps::with_event() to configure
/// a projector that processes catch-up events.
#[tokio::test]
async fn system_codec_integrates_with_projector_props() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let event_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("sys-proj-agg");

    // Pre-append one event so catch-up has something to project
    event_store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: EventType::from_str("SysIntIncremented"),
                payload: serde_json::to_vec(&Incremented).map(Bytes::from).unwrap(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: use system.codec() with ProjectorProps
    let _proxy = ProjectorProps::new(
        ProjectionId::new("sys-proj"),
        Arc::clone(&event_store),
        Arc::clone(&checkpoint_store),
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Incremented>(system.codec::<Incremented>())
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

// ---------------------------------------------------------------------------
// Test: aggregate_props wires codec and allows manual snapshot configuration
// ---------------------------------------------------------------------------

/// system.aggregate_props() returns a pre-wired AggregateProps.
/// Passing it the snapshot persistor and codec allows snapshotable aggregates
/// to restore state from a saved snapshot.
#[tokio::test]
async fn snapshot_roundtrip_via_event_source_system() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let event_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshot_store = Arc::new(InMemorySnapshotStore::default());
    let snapshot_ref = SnapshotPersistor::spawn(system.process_system(), snapshot_store).await;
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

    // When: spawn a process that restores from the snapshot using the system's codec
    let proxy = system
        .aggregate_props::<CounterWithSnapshot>(id, event_store)
        .with_snapshot_persistor(snapshot_ref, system.codec::<u64>())
        .spawn(system.process_system())
        .await;

    let count = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then: state is restored from snapshot (value=7)
    assert_eq!(
        count, 7,
        "snapshot restore must produce the saved value (7)"
    );
}
