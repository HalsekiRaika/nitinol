// E2E test: Aggregate replay after process restart.
//
// Scenario: Aggregate accumulates events → process "restarts" (same EventPersistorRef,
// new proxy) → on_start replays from the EventStore → state is fully restored.
//
// Three E2E stories:
//   1. Basic replay: N events → restart → state = N
//   2. Buffering: message sent during slow replay is processed after on_start completes
//   3. Snapshot-assisted replay: snapshot + delta events → state correct

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    AggregateProps, AggregateProxy, Snapshotable,
    codec::Codec, Aggregate, Context, Decider, Effect, Event,
    EventPersistor, EventPersistorRef,
    Receive as EvtReceive,
    SnapshotPersistor, SnapshotPersistorRef,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, AppendingEvent, AppendOutcome, EventType, LoadQuery, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("E2EReplay.Incremented");
}

// ---------------------------------------------------------------------------
// Fixtures: aggregate (no snapshot)
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
// Fixtures: aggregate (with Snapshotable)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SnapshotableCounter {
    value: u64,
}

impl Aggregate for SnapshotableCounter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for SnapshotableCounter {
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
impl Decider<Increment> for SnapshotableCounter {
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
impl EvtReceive<GetCount> for SnapshotableCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: pass-through codec for Incremented (no payload to encode)
// ---------------------------------------------------------------------------

struct TestCodec;

impl Codec<Incremented> for TestCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: big-endian u64 codec for u64 snapshots
// ---------------------------------------------------------------------------

struct BigEndianU64Codec;

#[derive(Debug, thiserror::Error)]
#[error("snapshot payload must be 8 bytes, got {0}")]
struct U64DecodeError(usize);

impl Codec<u64> for BigEndianU64Codec {
    type Error = U64DecodeError;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        payload
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| U64DecodeError(payload.len()))
    }
}

// ---------------------------------------------------------------------------
// Fixtures: SlowEventStore — wraps InMemoryEventStore with a load delay
// ---------------------------------------------------------------------------

/// Simulates slow storage by delaying `load` calls.
/// Used to verify that messages sent during replay are buffered until on_start completes.
struct SlowEventStore {
    inner: Arc<InMemoryEventStore>,
    load_delay: Duration,
}

#[async_trait]
impl EventStore for SlowEventStore {
    async fn append(
        &self,
        aggregate_id: &AggregateId,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(aggregate_id, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        tokio::time::sleep(self.load_delay).await;
        self.inner.load(query).await
    }
}

// ---------------------------------------------------------------------------
// Helpers: spawn helpers
// ---------------------------------------------------------------------------

async fn spawn_counter(
    system: &ProcessSystem,
    id: AggregateId,
    event_ref: EventPersistorRef,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id, event_ref)
        .with_codec(Arc::new(TestCodec))
        .spawn(system)
        .await
}

async fn spawn_snapshotable_counter(
    system: &ProcessSystem,
    id: AggregateId,
    event_ref: EventPersistorRef,
    snapshot_ref: SnapshotPersistorRef,
) -> AggregateProxy<SnapshotableCounter> {
    AggregateProps::<SnapshotableCounter>::new(id, event_ref)
        .with_codec(Arc::new(TestCodec))
        .with_snapshot_persistor(snapshot_ref, Arc::new(BigEndianU64Codec))
        .spawn(system)
        .await
}

// ---------------------------------------------------------------------------
// Test 1: replay restores state after process restart
// ---------------------------------------------------------------------------

/// Given three ask(Increment) calls via process 1 (3 events persisted),
/// When a second process is spawned for the same AggregateId,
/// Then on_start replays all 3 events and restores counter to 3.
#[tokio::test]
async fn e2e_replay_restores_state_after_process_restart() {
    // Given: shared system and event persistor
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let id = AggregateId::new("e2e-replay-basic");

    // Process 1: write 3 events
    {
        let proxy1 = spawn_counter(&system, id.clone(), event_ref.clone()).await;
        proxy1.ask(Increment).await.expect("ask 1");
        proxy1.ask(Increment).await.expect("ask 2");
        proxy1.ask(Increment).await.expect("ask 3");
    }

    // When: spawn process 2 (triggers replay in on_start)
    let proxy2 = spawn_counter(&system, id, event_ref).await;
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state fully restored from replay
    assert_eq!(count, 3, "replay must restore counter to 3 after 3 persisted events");
}

// ---------------------------------------------------------------------------
// Test 2: message sent during slow replay is processed after on_start
// ---------------------------------------------------------------------------

/// Given a SlowEventStore that delays load() by 50 ms,
/// and 2 events pre-written to the inner store,
/// When a process is spawned (on_start enters slow replay) AND exec(GetCount) is called
/// immediately (before on_start finishes),
/// Then the exec is buffered in the mpsc channel and processed after replay completes,
/// returning count == 2.
///
/// This verifies the runtime invariant: on_start runs to completion before the
/// message loop starts (lifecycle.rs), so buffered messages are guaranteed to see
/// the fully-replayed state.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_message_sent_during_slow_replay_is_processed_after() {
    // Given: pre-write 2 events to the inner store
    let inner = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("e2e-replay-buffer");

    {
        let setup_system = ProcessSystem::new().await;
        let event_ref = EventPersistor::spawn(
            &setup_system,
            Arc::clone(&inner) as Arc<dyn EventStore>,
        )
        .await;
        let proxy = spawn_counter(&setup_system, id.clone(), event_ref).await;
        proxy.ask(Increment).await.expect("setup ask 1");
        proxy.ask(Increment).await.expect("setup ask 2");
    }

    // Wrap the inner store in a slow store (50 ms load delay)
    let slow_store: Arc<dyn EventStore> = Arc::new(SlowEventStore {
        inner: Arc::clone(&inner),
        load_delay: Duration::from_millis(50),
    });

    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, slow_store).await;

    // When: spawn process (on_start begins the 50 ms slow replay)
    let proxy = spawn_counter(&system, id, event_ref).await;

    // exec is queued while on_start is still sleeping; runtime buffers it
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then: state reflects both replayed events
    assert_eq!(
        count, 2,
        "exec buffered during slow replay must see the fully-replayed state (count=2)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: replay with snapshot restores state correctly
// ---------------------------------------------------------------------------

/// Given a snapshot at sequence=3 (value=3) followed by 2 additional events (seq=4,5),
/// When a new process is spawned for the same AggregateId,
/// Then on_start loads the snapshot, applies the 2 delta events, and restores counter to 5.
#[tokio::test]
async fn e2e_replay_with_snapshot_restores_state() {
    // Given: shared persistors
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("e2e-replay-snap");

    // Write 5 events through process 1
    {
        let proxy1 = spawn_snapshotable_counter(
            &system,
            id.clone(),
            event_ref.clone(),
            snapshot_ref.clone(),
        )
        .await;
        for _ in 0..5 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Save a snapshot at sequence=3 (value=3)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When: spawn process 2 — replay: snapshot (seq=3) + delta events (seq=4, seq=5)
    let proxy2 = spawn_snapshotable_counter(&system, id, event_ref, snapshot_ref).await;
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: snapshot (value=3) + 2 delta events = 5
    assert_eq!(
        count, 5,
        "replay must combine snapshot (3) with 2 delta events to produce count=5"
    );
}
