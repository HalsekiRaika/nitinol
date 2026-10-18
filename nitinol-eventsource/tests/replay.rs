// Integration tests for the Replay flow in AggregateProcess.
//
// These tests verify that on_start correctly restores Aggregate state from the
// EventStore — and, for an aggregate whose `Recover` says `Latest`, from the
// snapshot its system's own persistor holds — before the process begins serving
// user messages.
//
// A restart is written as a second `EventSourceSystem` over the same store: one
// system resolves one activation per identity, so a second spawn on it would
// reach the running aggregate and replay nothing, while a second system has a
// registry of its own — which is what a restarted node is.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, Latest,
    Never, Query, Snapshotable,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::store::{
    Album, EventStore, EventStream, InMemoryEventStore, InMemorySnapshotStore, Journal,
};
use nitinol_persistence::LoadQuery;
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, PersistedSnapshot, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));

    type Codec = TestCodec;
}

// Fixtures: aggregate (no snapshot)

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: aggregate (with Snapshotable)

/// Counter that supports snapshot capture and restore.
/// Snapshot value: the raw u64 counter value.
/// Byte encoding is handled by BigEndianU64Codec (see below).
#[derive(Default)]
struct SnapshotableCounter {
    value: u64,
}

impl Aggregate for SnapshotableCounter {
    type Event = Incremented;
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for SnapshotableCounter {
    type Snapshot = u64;

    type Codec = BigEndianU64Codec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// Fixtures: commands and messages

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

impl Decider<Increment> for SnapshotableCounter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for SnapshotableCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixtures: test codecs

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

// Fixtures: SlowEventStore — wraps an InMemoryEventStore with a load delay

/// Wraps InMemoryEventStore to introduce a delay in `load`, simulating slow storage.
/// Used to test that messages sent during replay are correctly buffered by the mpsc channel.
#[derive(Clone)]
struct SlowEventStore {
    inner: InMemoryEventStore,
    load_delay: Duration,
}

#[async_trait]
impl EventStore for SlowEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        tokio::time::sleep(self.load_delay).await;
        self.inner.load(query).await
    }
}

// Helpers

/// A node over `store`, with a registry and a journal of its own.
async fn node_over<St: Store>(store: &St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build()
}

/// A store holding both the journal and the snapshot slot a
/// `SnapshotableCounter` recovers through.
fn journal_and_snapshots() -> CustomStore<Album> {
    CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build()
}

/// Resolves the Counter `id` on `system`, which is where its journal comes from.
async fn spawn_counter<St: Store>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id).spawn(system).await
}

/// Resolves the SnapshotableCounter `id` on `system`.
///
/// Nothing is wired in for the recovery: the aggregate's own `Recover = Latest`
/// is what asks for the newest snapshot, and the persistor it is read through is
/// the one `system` distributes.
async fn spawn_snapshotable<St: SnapshotSlot>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<SnapshotableCounter> {
    AggregateProps::<SnapshotableCounter>::new(id)
        .spawn(system)
        .await
}

// Replay: all events applied from empty state

/// Append 3 events via process 1, then spawn process 2 on a second node over the
/// same store and aggregate_id. Replay in on_start restores the counter to 3.
#[tokio::test]
async fn replay_restores_state_from_persisted_events() {
    // Given: one event store, and a node over it
    let store = Journal::from(InMemoryEventStore::default());
    let system = node_over(&store).await;
    let id = AggregateId::new("replay-basic");

    // Write 3 events through process 1
    {
        let proxy = spawn_counter(&system, id.clone()).await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // When: a second node over the same store spawns the same aggregate_id
    // (triggers replay in on_start)
    let restarted = node_over(&store).await;
    let proxy2 = spawn_counter(&restarted, id).await;

    // Then: replayed state equals 3
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "replay must restore state to 3");
}

// Replay: empty store yields default state

/// Spawning a process when no events exist starts from the aggregate's Default state.
#[tokio::test]
async fn replay_from_empty_store_starts_with_default_state() {
    // Given: fresh event store with no events
    let store = Journal::from(InMemoryEventStore::default());
    let system = node_over(&store).await;

    // When
    let proxy = spawn_counter(&system, AggregateId::new("replay-empty")).await;

    // Then: default Counter::value == 0
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 0, "empty store must yield default state (value=0)");
}

// Replay with Snapshot: restore from snapshot, no delta events

/// When a snapshot is present and there are no later events, state is restored
/// from the snapshot alone.
#[tokio::test]
async fn replay_with_snapshot_only_restores_snapshot_state() {
    // Given: a node whose persistor holds a pre-saved snapshot (value=5, sequence=5)
    let store = journal_and_snapshots();
    let system = node_over(&store).await;
    let id = AggregateId::new("replay-snap-only");

    // Manually save a snapshot via the system's own persistor (big-endian u64 encoding)
    system
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(5u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When
    let proxy = spawn_snapshotable(&system, id).await;

    // Then: state restored from snapshot → value=5
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 5, "replay must restore value=5 from snapshot");
}

// Replay with Snapshot + delta events

/// Snapshot at sequence=3 (value=3) followed by 2 appended events.
/// Replay applies the snapshot first, then the 2 delta events → value=5.
#[tokio::test]
async fn replay_applies_delta_events_after_snapshot() {
    // Given: one store, and a node over it
    let store = journal_and_snapshots();
    let system = node_over(&store).await;
    let id = AggregateId::new("replay-snap-delta");

    // Write 5 events through a snapshotable process
    {
        let proxy = spawn_snapshotable(&system, id.clone()).await;
        for _ in 0..5 {
            proxy.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Manually save a snapshot representing state after 3 events (value=3, sequence=3)
    system
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: a second node spawns the aggregate — replay loads snapshot (seq=3),
    // then 2 delta events (seq=4,5)
    let restarted = node_over(&store).await;
    let proxy2 = spawn_snapshotable(&restarted, id).await;

    // Then: value=5 (3 from snapshot + 2 delta events)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 5,
        "replay must apply snapshot (3) + 2 delta events = 5"
    );
}

// Replay with Snapshot: snapshot has no subsequent events

/// Snapshot at the latest sequence; no events exist beyond it.
/// Replay restores from snapshot and finds no delta events to apply.
#[tokio::test]
async fn replay_snapshot_at_latest_sequence_no_delta_events() {
    // Given: pre-write 3 events through a process, then save a snapshot at seq=3
    let store = journal_and_snapshots();
    let system = node_over(&store).await;
    let id = AggregateId::new("replay-snap-latest");

    {
        let proxy = spawn_snapshotable(&system, id.clone()).await;
        proxy.ask(Increment).await.expect("ask 1");
        proxy.ask(Increment).await.expect("ask 2");
        proxy.ask(Increment).await.expect("ask 3");
    }

    // Save snapshot at sequence=3
    system
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot");

    // When: a second node spawns the aggregate — the snapshot covers all events,
    // so no delta events follow
    let restarted = node_over(&store).await;
    let proxy2 = spawn_snapshotable(&restarted, id).await;

    // Then: value=3 (snapshot only)
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 3,
        "replay must restore value=3 from snapshot with no delta events"
    );
}

// Buffering: messages sent during slow replay are processed after on_start

/// Messages sent to a newly spawned process while on_start is still running
/// (replay from a slow EventStore) are buffered in the mpsc channel and
/// processed after replay completes.
///
/// Invariant guaranteed by runtime lifecycle (lifecycle.rs:111):
/// `state.on_start(&mut ctx).await` runs to completion before the select! loop begins.
/// Messages arrive into the mpsc buffer (capacity=32) and are consumed in FIFO order.
#[tokio::test]
async fn messages_buffered_during_slow_replay_are_processed_after() {
    // Given: pre-write 2 events to the inner store
    let inner = InMemoryEventStore::default();
    let id = AggregateId::new("replay-buffering");

    {
        let store = Journal::from(inner.clone());
        let system = node_over(&store).await;
        let proxy = spawn_counter(&system, id.clone()).await;
        proxy.ask(Increment).await.expect("setup ask 1");
        proxy.ask(Increment).await.expect("setup ask 2");
        // system drops here — events remain in `inner`
    }

    // A slow store introduces a 50 ms delay in load so that replay takes measurable time.
    let slow_store = Journal::from(SlowEventStore {
        inner: inner.clone(),
        load_delay: Duration::from_millis(50),
    });

    let system2 = node_over(&slow_store).await;

    // When: spawn the process (on_start begins the 50ms slow replay via the store)
    let proxy2 = spawn_counter(&system2, id).await;

    // exec is queued immediately — it arrives while on_start is still in the 50ms sleep.
    // The runtime buffers it and processes it after on_start completes.
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state reflects the 2 replayed events
    assert_eq!(
        count, 2,
        "exec sent during replay must be processed after on_start; state must be 2"
    );
}
