// E2E test: Snapshot save → load_latest → recover → shortened replay.
//
// Scenario: Snapshotable aggregate accumulates events, a snapshot is saved,
// then a new activation recovers from the snapshot and applies only delta
// events.
//
// Three E2E stories:
//   1. Snapshot only: snapshot at the latest sequence → no delta events → state recovered
//   2. Snapshot + delta: snapshot at seq=N + M more events → state = N+M
//   3. No snapshot: all events replayed from scratch → correct state
//
// A restart is a second system over the same store: one system resolves one
// activation per identity, so asking the same system again would reach the
// resident state instead of recovering.

use bytes::Bytes;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, Latest,
    Never, Query, Snapshotable,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::store::{InMemoryEventStore, InMemorySnapshotStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, PersistedSnapshot, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.snap"), TypeName::new("Incremented"));

    type Codec = EventCodec;
}

// Fixtures: Snapshotable aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for Counter {
    type Snapshot = u64;

    type Codec = SnapshotCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// Fixtures: plain aggregate (no Snapshotable) — used for the no-snapshot test

#[derive(Default)]
struct PlainCounter {
    value: u64,
}

impl Aggregate for PlainCounter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
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

impl Decider<Increment> for PlainCounter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for PlainCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixtures: codecs

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

// Helpers

/// A system over `store`, on a process system of its own.
async fn system_over<St: Store>(store: St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// Spawn a Counter (Snapshotable) on `system`, which supplies both the journal
/// and the snapshot store its `Recover = Latest` asks for.
async fn spawn_counter<St: SnapshotSlot>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id).spawn(system).await
}

/// Spawn a PlainCounter (no Snapshotable) on `system`, which it asks for the
/// journal alone.
async fn spawn_plain_counter<St: Store>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<PlainCounter> {
    AggregateProps::<PlainCounter>::new(id).spawn(system).await
}

// Test 1: snapshot at latest sequence — no delta events → state recovered correctly

/// Given 3 events persisted and a snapshot saved at sequence=3 (value=3),
/// When a new activation starts (snapshot load, no delta events),
/// Then the counter is recovered to 3.
#[tokio::test]
async fn e2e_snapshot_at_latest_sequence_restores_state() {
    // Given: a store this test holds, and a system over it
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build();
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("e2e-snap-latest");

    // Write 3 events via activation 1
    {
        let proxy1 = spawn_counter(&writing, id.clone()).await;
        proxy1.ask(Increment).await.expect("ask 1");
        proxy1.ask(Increment).await.expect("ask 2");
        proxy1.ask(Increment).await.expect("ask 3");
    }

    // Save a snapshot at seq=3 (covers all events, no delta), through the
    // persistor that system distributes
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

    // When: a second system over the same store starts activation 2 — loads the
    // snapshot at seq=3 and finds no delta events
    let restarted = system_over(store).await;
    let proxy2 = spawn_counter(&restarted, id).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: state fully recovered from the snapshot alone
    assert_eq!(
        count, 3,
        "snapshot at latest sequence must restore counter to 3 with no delta events"
    );
}

// Test 2: snapshot + delta events → combined state

/// Given 8 events persisted (seq=1..8) and a snapshot at seq=5 (value=5),
/// When a new activation starts,
/// Then on_start loads the snapshot (value=5) and replays the 3 delta events (seq=6,7,8)
/// producing counter=8.
#[tokio::test]
async fn e2e_snapshot_plus_delta_events_combine_correctly() {
    // Given: a store this test holds, and a system over it
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build();
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("e2e-snap-delta");

    // Write 8 events
    {
        let proxy1 = spawn_counter(&writing, id.clone()).await;
        for _ in 0..8 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // Save a snapshot at seq=5 (value=5), through the persistor that system
    // distributes
    writing
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

    // When: a second system over the same store starts activation 2 — loads the
    // snapshot (seq=5) then replays seq=6,7,8
    let restarted = system_over(store).await;
    let proxy2 = spawn_counter(&restarted, id).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: snapshot (5) + 3 delta events = 8
    assert_eq!(
        count, 8,
        "snapshot (5) + 3 delta events must produce counter=8"
    );
}

// Test 3: no snapshot configured → full event replay from the beginning

/// Given 5 events persisted for a PlainCounter (whose `Recover` is `Never`),
/// When a new activation starts on a store that holds the journal alone,
/// Then all 5 events are replayed from scratch and the counter reaches 5.
#[tokio::test]
async fn e2e_no_snapshot_means_full_event_replay() {
    // Given: a journal and nothing else, so there is no snapshot to recover from
    let store = Journal::from(InMemoryEventStore::default());
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("e2e-snap-no-snap");

    // Write 5 events via activation 1
    {
        let proxy1 = spawn_plain_counter(&writing, id.clone()).await;
        for _ in 0..5 {
            proxy1.ask(Increment).await.expect("ask must succeed");
        }
    }

    // When: a second system over the same journal starts activation 2 — no
    // snapshot, so all 5 events are replayed
    let restarted = system_over(store).await;
    let proxy2 = spawn_plain_counter(&restarted, id).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: full replay produces correct state
    assert_eq!(
        count, 5,
        "without a snapshot, all 5 events must be replayed to produce counter=5"
    );
}
