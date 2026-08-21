// E2E test: Basic Aggregate lifecycle with InMemoryEventStore and JsonCodec.
//
// Scenario: Aggregate 1 + EventSourceSystem + InMemoryEventStore + JsonCodec.
// Flow: ask(Cmd) → Decider::decide → Effect::Persist → EventStore::append
//       → Aggregate::apply → Vec<Event> returned to caller.
//
// These tests verify the full user-facing entry point (EventSourceSystem) end-to-end.
// The key difference from unit tests (aggregate_process.rs, system_integration.rs) is
// that this file tells a single cohesive user story rather than testing individual
// features in isolation.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use futures_util::TryStreamExt;
use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event,
    Receive as EvtReceive,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.agg"), TypeName::new("Incremented"));
}

// Fixtures: aggregate

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

// Fixtures: commands and queries

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

// Fixtures: JsonCodec (serde_json-backed)

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

// Test 1: ask() returns the persisted event

/// Given a fresh Counter aggregate backed by InMemoryEventStore + JsonCodec,
/// When ask(Increment) is called,
/// Then the returned Vec equals [Incremented] — the single persisted event.
#[tokio::test]
async fn e2e_ask_persists_event_and_returns_it() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = system
        .spawn_aggregate::<Counter>(AggregateId::new("e2e-agg-ask"), store)
        .await;

    // When
    let events = proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        events,
        vec![Incremented],
        "ask(Increment) must return [Incremented]"
    );
}

// Test 2: a later reference observes the persisted state

/// Given one ask(Increment) was processed through a reference to this aggregate,
/// When a second reference for the same AggregateId is resolved later,
/// Then it observes the persisted event as a count of 1.
///
/// The caller cannot tell whether that reference activated the aggregate and
/// replayed the stream, or joined a live activation — resolve is deliberately
/// silent about which happened (R-3), and either way the state a caller sees is
/// the same.
#[tokio::test]
async fn e2e_persisted_state_is_visible_through_a_later_reference() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("e2e-agg-restart");

    // Reference 1: write one event via ask()
    {
        let proxy1 = system
            .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&store))
            .await;
        proxy1.ask(Increment).await.expect("ask must succeed");
    }

    // When: resolve the same AggregateId again
    let proxy2 = system.spawn_aggregate::<Counter>(id, store).await;
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");

    // Then: the later reference sees the persisted increment
    assert_eq!(
        count, 1,
        "a later reference must observe the one persisted Increment"
    );
}

// Test 3: multiple asks advance sequence monotonically

/// Given three sequential ask(Increment) calls on the same process,
/// When the counter state and stored events are inspected,
/// Then value == 3 and the event persistor holds exactly 3 events
/// with monotonically increasing sequences [1, 2, 3].
#[tokio::test]
async fn e2e_multiple_asks_advance_sequence_monotonically() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("e2e-agg-multi");
    let proxy = system
        .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&store))
        .await;

    // When: three sequential increments
    proxy.ask(Increment).await.expect("ask 1");
    proxy.ask(Increment).await.expect("ask 2");
    proxy.ask(Increment).await.expect("ask 3");
    let count = proxy.exec(GetCount).await.expect("exec");

    // Then: in-memory state reflects all three increments
    assert_eq!(count, 3, "counter must be 3 after three Increment commands");

    // And: the store holds exactly 3 events with monotonic sequences
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(&id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");
    assert_eq!(loaded.len(), 3, "three events must be persisted");

    let sequences: Vec<u64> = loaded.iter().map(|e| e.sequence).collect();
    assert_eq!(
        sequences,
        vec![1, 2, 3],
        "event sequences must be monotonically [1, 2, 3]"
    );
}
