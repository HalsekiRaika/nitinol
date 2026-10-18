// E2E test: Aggregate replay after a restart.
//
// Scenario: Aggregate accumulates events → the aggregate "restarts" (same
// store, a second system over it) → on_start replays from the EventStore →
// state is fully restored.
//
// A restart is a second system because one system resolves one activation per
// identity: asking the same system again reaches the state already in memory,
// which is nothing a replay produced.
//
// Three E2E stories:
//   1. Basic replay: N events → restart → state = N
//   2. Buffering: message sent during slow replay is processed after on_start completes
//   3. Snapshot-assisted replay: snapshot + delta events → state correct

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
    EventStore, EventStream, InMemoryEventStore, InMemorySnapshotStore, Journal,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot,
    TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.replay"), TypeName::new("Incremented"));

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

// Fixtures: pass-through codec for Incremented (no payload to encode)

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

// Fixtures: big-endian u64 codec for u64 snapshots

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

// Fixtures: SlowEventStore — wraps InMemoryEventStore with a load delay

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

// Helpers: a system, and the spawn helpers

/// A system over `store`, on a process system of its own.
async fn system_over<St: Store>(store: St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

async fn spawn_counter<St: Store>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id).spawn(system).await
}

/// `SnapshotableCounter` declares `Recover = Latest`, so the snapshot store it
/// recovers through is one more thing the system supplies.
async fn spawn_snapshotable_counter<St: SnapshotSlot>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<SnapshotableCounter> {
    AggregateProps::<SnapshotableCounter>::new(id)
        .spawn(system)
        .await
}

// Test 1: replay restores state after a restart

/// Given three ask(Increment) calls via activation 1 (3 events persisted),
/// When a second activation starts for the same AggregateId,
/// Then on_start replays all 3 events and restores counter to 3.
#[tokio::test]
async fn e2e_replay_restores_state_after_process_restart() {
    // Given: one event store, and a system over it
    let store = Journal::from(InMemoryEventStore::default());
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("e2e-replay-basic");

    // Activation 1: write 3 events
    {
        let proxy1 = spawn_counter(&writing, id.clone()).await;
        proxy1.ask(Increment).await.expect("ask 1");
        proxy1.ask(Increment).await.expect("ask 2");
        proxy1.ask(Increment).await.expect("ask 3");
    }

    // When: a second system over the same store starts activation 2 (triggers
    // replay in on_start)
    let restarted = system_over(store).await;
    let proxy2 = spawn_counter(&restarted, id).await;
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state fully restored from replay
    assert_eq!(
        count, 3,
        "replay must restore counter to 3 after 3 persisted events"
    );
}

// Test 2: message sent during slow replay is processed after on_start

/// Given a SlowEventStore that delays load() by 50 ms,
/// and 2 events pre-written to the inner store,
/// When an activation starts (on_start enters slow replay) AND exec(GetCount) is called
/// immediately (before on_start finishes),
/// Then the exec is buffered in the mpsc channel and processed after replay completes,
/// returning count == 2.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_message_sent_during_slow_replay_is_processed_after() {
    // Given: pre-write 2 events to the inner store
    let inner = InMemoryEventStore::default();
    let id = AggregateId::new("e2e-replay-buffer");

    {
        let setup = system_over(Journal::from(inner.clone())).await;
        let proxy = spawn_counter(&setup, id.clone()).await;
        proxy.ask(Increment).await.expect("setup ask 1");
        proxy.ask(Increment).await.expect("setup ask 2");
    }

    // The restart reads the same records through a store that delays every load
    // by 50 ms, so on_start is still replaying when the exec below is sent
    let slow = system_over(Journal::from(SlowEventStore {
        inner: inner.clone(),
        load_delay: Duration::from_millis(50),
    }))
    .await;

    // When: the activation starts (on_start begins the 50 ms slow replay)
    let proxy = spawn_counter(&slow, id).await;

    // exec is queued while on_start is still sleeping; runtime buffers it
    let count: u64 = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then: state reflects both replayed events
    assert_eq!(
        count, 2,
        "exec buffered during slow replay must see the fully-replayed state (count=2)"
    );
}

// Test 3: replay with snapshot restores state correctly

/// Given a snapshot at sequence=3 (value=3) followed by 2 additional events (seq=4,5),
/// When a new activation starts for the same AggregateId,
/// Then on_start loads the snapshot, applies the 2 delta events, and restores counter to 5.
#[tokio::test]
async fn e2e_replay_with_snapshot_restores_state() {
    // Given: a store holding a journal and snapshots, and a system over it
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build();
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("e2e-replay-snap");

    // Write 5 events through activation 1
    {
        let proxy1 = spawn_snapshotable_counter(&writing, id.clone()).await;
        for _ in 0..5 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Save a snapshot at sequence=3 (value=3), through the persistor that
    // system distributes
    writing
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 3,
            payload: Bytes::from(3u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("save snapshot must succeed");

    // When: a second system over the same store starts activation 2 — replay:
    // snapshot (seq=3) + delta events (seq=4, seq=5)
    let restarted = system_over(store).await;
    let proxy2 = spawn_snapshotable_counter(&restarted, id).await;
    let count: u64 = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: snapshot (value=3) + 2 delta events = 5
    assert_eq!(
        count, 5,
        "replay must combine snapshot (3) with 2 delta events to produce count=5"
    );
}
