// E2E test: Snapshot save → load_latest → restore → shortened replay.
//
// Scenario: Snapshotable aggregate accumulates events, a snapshot is saved,
// then a new process restores from the snapshot and applies only delta events.
//
// Three E2E stories:
//   1. Snapshot only: snapshot at the latest sequence → no delta events → state restored
//   2. Snapshot + delta: snapshot at seq=N + M more events → state = N+M
//   3. No snapshot: all events replayed from scratch → correct state

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, Context, Decider, Effect, Event,
    Receive as EvtReceive, SnapshotPersistor, SnapshotPersistorProxy, Snapshotable,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new("e2e.snap"), TypeName::new("Incremented"));
}

// ---------------------------------------------------------------------------
// Fixtures: Snapshotable aggregate
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

impl Snapshotable for Counter {
    type Snapshot = u64;

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// ---------------------------------------------------------------------------
// Fixtures: plain aggregate (no Snapshotable) — used for the no-snapshot test
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PlainCounter {
    value: u64,
}

impl Aggregate for PlainCounter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
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
impl Decider<Increment> for PlainCounter {
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
impl EvtReceive<GetCount> for PlainCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: codecs
// ---------------------------------------------------------------------------

/// Pass-through codec for Incremented (unit struct — no payload to encode).
struct EventCodec;

impl Codec<Incremented> for EventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

/// Big-endian u64 codec for the Counter snapshot (raw u64 value).
struct SnapshotCodec;

#[derive(Debug, thiserror::Error)]
#[error("snapshot payload must be 8 bytes, got {0}")]
struct SnapshotDecodeError(usize);

impl Codec<u64> for SnapshotCodec {
    type Error = SnapshotDecodeError;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        payload
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| SnapshotDecodeError(payload.len()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a Counter (Snapshotable) process with event store + snapshot persistor.
async fn spawn_counter(
    system: &ProcessSystem,
    id: AggregateId,
    store: Arc<dyn EventStore>,
    snapshot_ref: SnapshotPersistorProxy,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id, store)
        .with_codec(Arc::new(EventCodec))
        .with_snapshot_persistor(snapshot_ref, Arc::new(SnapshotCodec))
        .spawn(system)
        .await
}

/// Spawn a PlainCounter (no Snapshotable) process with event store only.
async fn spawn_plain_counter(
    system: &ProcessSystem,
    id: AggregateId,
    store: Arc<dyn EventStore>,
) -> AggregateProxy<PlainCounter> {
    AggregateProps::<PlainCounter>::new(id, store)
        .with_codec(Arc::new(EventCodec))
        .spawn(system)
        .await
}

// ---------------------------------------------------------------------------
// Test 1: snapshot at latest sequence — no delta events → state restored correctly
// ---------------------------------------------------------------------------

/// Given 3 events persisted and a snapshot saved at sequence=3 (value=3),
/// When a new process is spawned (snapshot load, no delta events),
/// Then the counter is restored to 3.
#[tokio::test]
async fn e2e_snapshot_at_latest_sequence_restores_state() {
    // Given: shared system and persistors
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("e2e-snap-latest");

    // Write 3 events via process 1
    {
        let proxy1 = spawn_counter(
            &system,
            id.clone(),
            Arc::clone(&store),
            snapshot_ref.clone(),
        )
        .await;
        proxy1.ask(Increment).await.expect("ask 1");
        proxy1.ask(Increment).await.expect("ask 2");
        proxy1.ask(Increment).await.expect("ask 3");
    }

    // Save a snapshot at seq=3 (covers all events, no delta)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When: spawn process 2 — loads snapshot at seq=3, finds no delta events
    let proxy2 = spawn_counter(&system, id, store, snapshot_ref).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state fully restored from the snapshot alone
    assert_eq!(
        count, 3,
        "snapshot at latest sequence must restore counter to 3 with no delta events"
    );
}

// ---------------------------------------------------------------------------
// Test 2: snapshot + delta events → combined state
// ---------------------------------------------------------------------------

/// Given 8 events persisted (seq=1..8) and a snapshot at seq=5 (value=5),
/// When a new process is spawned,
/// Then on_start loads the snapshot (value=5) and replays the 3 delta events (seq=6,7,8)
/// producing counter=8.
#[tokio::test]
async fn e2e_snapshot_plus_delta_events_combine_correctly() {
    // Given: shared system and persistors
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("e2e-snap-delta");

    // Write 8 events
    {
        let proxy1 = spawn_counter(
            &system,
            id.clone(),
            Arc::clone(&store),
            snapshot_ref.clone(),
        )
        .await;
        for _ in 0..8 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Save a snapshot at seq=5 (value=5)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(5u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When: spawn process 2 — loads snapshot (seq=5) then replays seq=6,7,8
    let proxy2 = spawn_counter(&system, id, store, snapshot_ref).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: snapshot (5) + 3 delta events = 8
    assert_eq!(
        count, 8,
        "snapshot (5) + 3 delta events must produce counter=8"
    );
}

// ---------------------------------------------------------------------------
// Test 3: no snapshot configured → full event replay from the beginning
// ---------------------------------------------------------------------------

/// Given 5 events persisted for a PlainCounter (no Snapshotable),
/// When a new process is spawned (event persistor only, no snapshot persistor),
/// Then all 5 events are replayed from scratch and the counter reaches 5.
#[tokio::test]
async fn e2e_no_snapshot_means_full_event_replay() {
    // Given: event store only (no snapshot store)
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("e2e-snap-no-snap");

    // Write 5 events via process 1
    {
        let proxy1 = spawn_plain_counter(&system, id.clone(), Arc::clone(&store)).await;
        for _ in 0..5 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // When: spawn process 2 — no snapshot, all 5 events are replayed
    let proxy2 = spawn_plain_counter(&system, id, store).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: full replay produces correct state
    assert_eq!(
        count, 5,
        "without a snapshot, all 5 events must be replayed to produce counter=5"
    );
}
