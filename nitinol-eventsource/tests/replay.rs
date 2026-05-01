// Integration tests for the Replay flow in AggregateProcess.
//
// These tests verify that on_start correctly restores Aggregate state from an
// EventStore (and optionally a SnapshotStore) before the process begins serving
// user messages.
//
// Expected failures before Phase 2.4 implementation:
//  - Missing types: AggregateProps, AggregateProxy, EventCodec, EncodeError, DecodeError,
//    AskError, ExecError
//  - Missing dependency: jiff (added to [dependencies] in the implement step)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    Aggregate, Context, Decider, Effect, Event,
    Receive as EvtReceive,
    Snapshotable, SnapshotCaptureError, SnapshotRestoreError,
    AggregateProps, AggregateProxy,
    EventCodec, EncodeError, DecodeError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemorySnapshotStore, SnapshotStore};
use nitinol_persistence::{AggregateId, EventType, PersistedSnapshot};
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
/// Snapshot payload: 8 big-endian bytes encoding the u64 value.
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
    fn restore(payload: &[u8]) -> Result<Self, SnapshotRestoreError> {
        let arr: [u8; 8] = payload
            .try_into()
            .map_err(|e: std::array::TryFromSliceError| SnapshotRestoreError::Decode(Box::new(e)))?;
        Ok(Self { value: u64::from_be_bytes(arr) })
    }

    fn capture(&self) -> Result<Bytes, SnapshotCaptureError> {
        Ok(Bytes::from(self.value.to_be_bytes().to_vec()))
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
// Fixtures: test codec
// ---------------------------------------------------------------------------

struct TestCodec;

impl EventCodec<Incremented> for TestCodec {
    fn encode(&self, _event: &Incremented) -> Result<Bytes, EncodeError> {
        Ok(Bytes::new())
    }

    fn decode(&self, _event_type: EventType, _bytes: Bytes) -> Result<Incremented, DecodeError> {
        Ok(Incremented)
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
        events: Vec<nitinol_persistence::AppendingEvent>,
    ) -> Result<nitinol_persistence::AppendOutcome, nitinol_persistence::error::AppendError> {
        self.inner.append(aggregate_id, events).await
    }

    async fn load(
        &self,
        query: nitinol_persistence::LoadQuery,
    ) -> Result<nitinol_persistence::store::EventStream<'_>, nitinol_persistence::error::LoadError> {
        tokio::time::sleep(self.load_delay).await;
        self.inner.load(query).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawns a fresh Counter process using the given event store.
async fn spawn_counter(
    id: AggregateId,
    store: Arc<dyn EventStore>,
) -> (ProcessSystem, AggregateProxy<Counter>) {
    let system = ProcessSystem::new().await;
    let proxy = AggregateProps::<Counter>::new(id, store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;
    (system, proxy)
}

/// Spawns a SnapshotableCounter process, optionally with a snapshot store.
async fn spawn_snapshotable(
    id: AggregateId,
    event_store: Arc<dyn EventStore>,
    snapshot_store: Arc<dyn SnapshotStore>,
) -> (ProcessSystem, AggregateProxy<SnapshotableCounter>) {
    let system = ProcessSystem::new().await;
    let proxy = AggregateProps::<SnapshotableCounter>::new(id, event_store)
        .with_codec(Arc::new(TestCodec))
        .with_snapshot_store(snapshot_store)
        .spawn(&system)
        .await;
    (system, proxy)
}

// ---------------------------------------------------------------------------
// Replay: all events applied from empty state
// ---------------------------------------------------------------------------

/// Append 3 events via process 1, then spawn process 2 with the same aggregate_id
/// and event store. Replay in on_start restores the counter to 3.
#[tokio::test]
async fn replay_restores_state_from_persisted_events() {
    // Given: write 3 events through process 1
    let inner = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("replay-basic");
    {
        let (_sys, proxy) = spawn_counter(id.clone(), Arc::clone(&inner) as Arc<dyn EventStore>).await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // When: spawn a fresh process for the same aggregate_id (triggers replay in on_start)
    let (_sys2, proxy2) = spawn_counter(id, Arc::clone(&inner) as Arc<dyn EventStore>).await;

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
    // Given: fresh event store with no events
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When
    let (_sys, proxy) = spawn_counter(AggregateId::new("replay-empty"), store).await;

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
    // Given: snapshot encoding value=5 at sequence=5
    let event_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let snapshot_store = Arc::new(InMemorySnapshotStore::default());
    let id = AggregateId::new("replay-snap-only");

    let snapshot_payload = Bytes::from(5u64.to_be_bytes().to_vec());
    snapshot_store
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: snapshot_payload,
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When
    let (_sys, proxy) = spawn_snapshotable(
        id,
        event_store,
        Arc::clone(&snapshot_store) as Arc<dyn SnapshotStore>,
    )
    .await;

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
    // Given: write 5 events via a snapshotable process, save a snapshot at seq=3,
    // then write 2 more events.
    let inner = Arc::new(InMemoryEventStore::default());
    let snapshot_store = Arc::new(InMemorySnapshotStore::default());
    let id = AggregateId::new("replay-snap-delta");

    {
        // Write 5 events
        let (_sys, proxy) = spawn_snapshotable(
            id.clone(),
            Arc::clone(&inner) as Arc<dyn EventStore>,
            Arc::clone(&snapshot_store) as Arc<dyn SnapshotStore>,
        )
        .await;
        for _ in 0..5 {
            proxy.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Manually save a snapshot representing state after 3 events (value=3, sequence=3)
    snapshot_store
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: spawn a fresh process — replay loads snapshot (seq=3), then 2 delta events (seq=4,5)
    let (_sys2, proxy2) = spawn_snapshotable(
        id,
        Arc::clone(&inner) as Arc<dyn EventStore>,
        Arc::clone(&snapshot_store) as Arc<dyn SnapshotStore>,
    )
    .await;

    // Then: value=5 (3 from snapshot + 2 delta events)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 5, "replay must apply snapshot (3) + 2 delta events = 5");
}

// ---------------------------------------------------------------------------
// Replay with Snapshot: snapshot has no subsequent events
// (identical to replay_with_snapshot_only but added for clarity)
// ---------------------------------------------------------------------------

/// Snapshot at the latest sequence; no events exist beyond it.
/// Replay restores from snapshot and finds no delta events to apply.
#[tokio::test]
async fn replay_snapshot_at_latest_sequence_no_delta_events() {
    // Given: pre-write 3 events through a process, then take a snapshot
    let inner = Arc::new(InMemoryEventStore::default());
    let snapshot_store = Arc::new(InMemorySnapshotStore::default());
    let id = AggregateId::new("replay-snap-latest");

    {
        let (_sys, proxy) = spawn_snapshotable(
            id.clone(),
            Arc::clone(&inner) as Arc<dyn EventStore>,
            Arc::clone(&snapshot_store) as Arc<dyn SnapshotStore>,
        )
        .await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // Save snapshot at sequence=3
    snapshot_store
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: spawn fresh process — snapshot covers all events, no delta events follow
    let (_sys2, proxy2) = spawn_snapshotable(
        id,
        Arc::clone(&inner) as Arc<dyn EventStore>,
        Arc::clone(&snapshot_store) as Arc<dyn SnapshotStore>,
    )
    .await;

    // Then: value=3 (snapshot only)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "replay must restore value=3 from snapshot with no delta events");
}

// ---------------------------------------------------------------------------
// Buffering: messages sent during slow replay are processed after on_start
// ---------------------------------------------------------------------------

/// Messages sent to a newly spawned process while on_start is still running
/// (replay from a slow EventStore) are buffered in the mpsc channel and processed
/// after replay completes.
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
        let (_sys, proxy) = spawn_counter(id.clone(), Arc::clone(&inner) as Arc<dyn EventStore>).await;
        proxy.ask(Increment).await.expect("setup ask 1");
        proxy.ask(Increment).await.expect("setup ask 2");
    }

    // A slow store introduces a 50 ms delay in load so that replay takes measurable time.
    let slow_store: Arc<dyn EventStore> = Arc::new(SlowEventStore {
        inner: Arc::clone(&inner),
        load_delay: Duration::from_millis(50),
    });

    let system2 = ProcessSystem::new().await;

    // When: spawn the process (on_start begins the 50ms slow replay)
    let proxy2 = AggregateProps::<Counter>::new(id, slow_store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system2)
        .await;

    // exec is queued immediately — it arrives while on_start is still in the 50ms sleep.
    // The runtime buffers it and processes it after on_start completes.
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state reflects the 2 replayed events
    assert_eq!(
        count, 2,
        "exec sent during replay must be processed after on_start; state must be 2"
    );
}
