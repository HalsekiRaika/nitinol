// Integration tests for the Replay flow in AggregateProcess.
//
// These tests verify that on_start correctly restores Aggregate state from an
// EventPersistor (and optionally a SnapshotPersistor) before the process begins
// serving user messages.
//
// Phase 2 redesign: the stores are no longer passed directly into AggregateProps.
// Instead, EventPersistor and SnapshotPersistor actors own the stores and receive
// messages from AggregateProcess via EventPersistorRef / SnapshotPersistorRef.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    Aggregate, codec::Codec, Context, Decider, Effect, Event,
    EventPersistor, EventPersistorRef,
    SnapshotPersistor, SnapshotPersistorRef,
    Receive as EvtReceive,
    Snapshotable,
    AggregateProps, AggregateProxy,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, AppendingEvent, AppendOutcome, EventType, PersistedSnapshot};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::EventStream;
use nitinol_persistence::LoadQuery;
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("Incremented");
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

/// Counter that supports snapshot capture and restore.
/// Snapshot value: the raw u64 counter value.
/// Byte encoding is handled by BigEndianU64Codec (see below).
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
// Fixtures: commands and messages
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
// Fixtures: test codecs
// ---------------------------------------------------------------------------

/// Pass-through codec for Incremented (unit struct — no data to encode).
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

/// Codec for u64 snapshots using 8-byte big-endian encoding.
/// This matches the encoding used when manually creating PersistedSnapshot payloads
/// in tests (e.g., `Bytes::from(5u64.to_be_bytes().to_vec())`).
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
        let arr: [u8; 8] = payload
            .try_into()
            .map_err(|_| U64DecodeError(payload.len()))?;
        Ok(u64::from_be_bytes(arr))
    }
}

// ---------------------------------------------------------------------------
// Fixtures: SlowEventStore — wraps an InMemoryEventStore with a load delay
// ---------------------------------------------------------------------------

/// Wraps InMemoryEventStore to introduce a delay in `load`, simulating slow storage.
/// Used to test that messages sent during replay are correctly buffered by the mpsc channel.
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

    async fn load(
        &self,
        query: LoadQuery,
    ) -> Result<EventStream<'_>, nitinol_persistence::error::LoadError> {
        tokio::time::sleep(self.load_delay).await;
        self.inner.load(query).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawns a fresh Counter process using the provided EventPersistorRef.
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

/// Spawns a SnapshotableCounter process using the provided EventPersistorRef and SnapshotPersistorRef.
async fn spawn_snapshotable(
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
// Replay: all events applied from empty state
// ---------------------------------------------------------------------------

/// Append 3 events via process 1, then spawn process 2 with the same EventPersistorRef
/// and aggregate_id. Replay in on_start restores the counter to 3.
#[tokio::test]
async fn replay_restores_state_from_persisted_events() {
    // Given: shared system and event persistor
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let id = AggregateId::new("replay-basic");

    // Write 3 events through process 1
    {
        let proxy = spawn_counter(&system, id.clone(), event_ref.clone()).await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // When: spawn a fresh process for the same aggregate_id (triggers replay in on_start)
    let proxy2 = spawn_counter(&system, id, event_ref).await;

    // Then: replayed state equals 3
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "replay must restore state to 3");
}

// ---------------------------------------------------------------------------
// Replay: empty store yields default state
// ---------------------------------------------------------------------------

/// Spawning a process when no events exist starts from the aggregate's Default state.
#[tokio::test]
async fn replay_from_empty_store_starts_with_default_state() {
    // Given: fresh event persistor with no events
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;

    // When
    let proxy = spawn_counter(&system, AggregateId::new("replay-empty"), event_ref).await;

    // Then: default Counter::value == 0
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 0, "empty store must yield default state (value=0)");
}

// ---------------------------------------------------------------------------
// Replay with Snapshot: restore from snapshot, no delta events
// ---------------------------------------------------------------------------

/// When a snapshot is present and there are no later events, state is restored
/// from the snapshot alone.
#[tokio::test]
async fn replay_with_snapshot_only_restores_snapshot_state() {
    // Given: a SnapshotPersistor with a pre-saved snapshot (value=5, sequence=5)
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("replay-snap-only");

    // Manually save a snapshot via SnapshotPersistorRef (big-endian u64 encoding)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(5u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When
    let proxy = spawn_snapshotable(&system, id, event_ref, snapshot_ref).await;

    // Then: state restored from snapshot → value=5
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 5, "replay must restore value=5 from snapshot");
}

// ---------------------------------------------------------------------------
// Replay with Snapshot + delta events
// ---------------------------------------------------------------------------

/// Snapshot at sequence=3 (value=3) followed by 2 appended events.
/// Replay applies the snapshot first, then the 2 delta events → value=5.
#[tokio::test]
async fn replay_applies_delta_events_after_snapshot() {
    // Given: shared persistors
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("replay-snap-delta");

    // Write 5 events through a snapshotable process
    {
        let proxy = spawn_snapshotable(
            &system,
            id.clone(),
            event_ref.clone(),
            snapshot_ref.clone(),
        )
        .await;
        for _ in 0..5 {
            proxy.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Manually save a snapshot representing state after 3 events (value=3, sequence=3)
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: spawn a fresh process — replay loads snapshot (seq=3), then 2 delta events (seq=4,5)
    let proxy2 = spawn_snapshotable(&system, id, event_ref, snapshot_ref).await;

    // Then: value=5 (3 from snapshot + 2 delta events)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 5, "replay must apply snapshot (3) + 2 delta events = 5");
}

// ---------------------------------------------------------------------------
// Replay with Snapshot: snapshot has no subsequent events
// ---------------------------------------------------------------------------

/// Snapshot at the latest sequence; no events exist beyond it.
/// Replay restores from snapshot and finds no delta events to apply.
#[tokio::test]
async fn replay_snapshot_at_latest_sequence_no_delta_events() {
    // Given: pre-write 3 events through a process, then save a snapshot at seq=3
    let system = ProcessSystem::new().await;
    let event_ref = EventPersistor::spawn(&system, Arc::new(InMemoryEventStore::default())).await;
    let snapshot_ref =
        SnapshotPersistor::spawn(&system, Arc::new(InMemorySnapshotStore::default())).await;
    let id = AggregateId::new("replay-snap-latest");

    {
        let proxy = spawn_snapshotable(
            &system,
            id.clone(),
            event_ref.clone(),
            snapshot_ref.clone(),
        )
        .await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // Save snapshot at sequence=3
    snapshot_ref
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: spawn fresh process — snapshot covers all events, no delta events follow
    let proxy2 = spawn_snapshotable(&system, id, event_ref, snapshot_ref).await;

    // Then: value=3 (snapshot only)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "replay must restore value=3 from snapshot with no delta events");
}

// ---------------------------------------------------------------------------
// Buffering: messages sent during slow replay are processed after on_start
// ---------------------------------------------------------------------------

/// Messages sent to a newly spawned process while on_start is still running
/// (replay from a slow EventStore passed into EventPersistor) are buffered in
/// the mpsc channel and processed after replay completes.
///
/// Invariant guaranteed by runtime lifecycle (lifecycle.rs:111):
/// `state.on_start(&mut ctx).await` runs to completion before the select! loop begins.
/// Messages arrive into the mpsc buffer (capacity=32) and are consumed in FIFO order.
#[tokio::test]
async fn messages_buffered_during_slow_replay_are_processed_after() {
    // Given: pre-write 2 events to the inner store
    let inner = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("replay-buffering");

    {
        let system = ProcessSystem::new().await;
        let event_ref = EventPersistor::spawn(&system, inner.clone()).await;
        let proxy = spawn_counter(&system, id.clone(), event_ref).await;
        proxy.ask(Increment).await.expect("setup ask 1");
        proxy.ask(Increment).await.expect("setup ask 2");
        // system and event_ref drop here — events remain in `inner`
    }

    // A slow store introduces a 50 ms delay in load so that replay takes measurable time.
    let slow_store: Arc<dyn EventStore> = Arc::new(SlowEventStore {
        inner: Arc::clone(&inner),
        load_delay: Duration::from_millis(50),
    });

    let system2 = ProcessSystem::new().await;
    let event_ref2 = EventPersistor::spawn(&system2, slow_store).await;

    // When: spawn the process (on_start begins the 50ms slow replay via EventPersistor)
    let proxy2 = spawn_counter(&system2, id, event_ref2).await;

    // exec is queued immediately — it arrives while on_start is still in the 50ms sleep.
    // The runtime buffers it and processes it after on_start completes.
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state reflects the 2 replayed events
    assert_eq!(
        count, 2,
        "exec sent during replay must be processed after on_start; state must be 2"
    );
}
