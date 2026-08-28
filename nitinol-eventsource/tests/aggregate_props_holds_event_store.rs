//! `AggregateProps::new` accepts `Arc<dyn EventStore>` directly.
//!
//! Replaces the previous `EventPersistor` actor wiring.  The aggregate
//! process now holds the store inline and calls `store.append` /
//! `store.load` without crossing an actor boundary.
//!
//! Two complementary checks:
//!
//! 1. Compile-time type assertion — `AggregateProps::new` must accept
//!    `Arc<dyn EventStore>` as its second argument.  Any regression to the
//!    old proxy-based signature fails to compile.
//! 2. Runtime integration — spawning + ask must succeed end-to-end with
//!    the new direct-store wiring (no actor in between).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Context, Decider, Effect, Event, Query,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Minimal counter aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));
}

impl Aggregate for Counter {
    type Event = Incremented;
    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

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

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

struct PassThroughCodec;

impl Codec<Incremented> for PassThroughCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

// Compile-time type assertion

/// `AggregateProps::<Counter>::new` MUST have the signature
/// `fn(AggregateId, Arc<dyn EventStore>) -> AggregateProps<Counter>`.
///
/// Not a `#[test]` — the compiler enforces the signature at build time.
/// A regression to `EventPersistorProxy` (or any other type) causes a
/// compile error.
#[allow(dead_code)]
fn _assert_sig_aggregate_props_new_accepts_arc_dyn_event_store() {
    let _: fn(AggregateId, Arc<dyn EventStore>) -> AggregateProps<Counter> = AggregateProps::new;
}

// Runtime integration: spawn + ask succeeds with Arc<dyn EventStore>

/// Spawning an AggregateProcess with Arc<dyn EventStore> succeeds, and a
/// command flows through `Effect::Persist` to `store.append` without going
/// through an actor proxy.
#[tokio::test]
async fn aggregate_props_spawns_with_arc_dyn_event_store() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When: AggregateProps takes the store directly (no EventPersistor::spawn)
    let proxy = AggregateProps::<Counter>::new(AggregateId::new("direct-store-1"), store)
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    // Then: ask succeeds — the process appends via the held store
    let events = proxy.ask(Increment).await.expect("ask must succeed");
    assert_eq!(events, vec![Incremented]);
}

// Runtime integration: state replay via direct store

/// After ask(Increment) on process 1, a second process sharing the same
/// Arc<dyn EventStore> replays the stored event in on_start and observes
/// the restored state.
///
/// This is the key behavior the refactor preserves: replay still works,
/// but now without an EventPersistor actor in between.
#[tokio::test]
async fn aggregate_replays_state_via_shared_arc_dyn_event_store() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("direct-store-replay");

    // Write 2 events through process 1
    {
        let proxy = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
            .with_codec(Arc::new(PassThroughCodec))
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask 1 must succeed");
        proxy.ask(Increment).await.expect("ask 2 must succeed");
    }

    // When: spawn a fresh process for the same id with the same store
    let proxy2 = AggregateProps::<Counter>::new(id, store)
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    // Then: replayed state == 2
    let count = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 2,
        "replay via direct-store wiring must restore state to 2"
    );
}

// Multiple AggregateProcess can share one Arc<dyn EventStore>

/// Two AggregateProcess instances on different ids share the same
/// Arc<dyn EventStore>.  Without the EventPersistor actor, the store itself
/// is the synchronization point — its own thread-safe internals (Mutex)
/// guarantee correctness.
#[tokio::test]
async fn multiple_aggregate_processes_share_single_arc_dyn_event_store() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When: spawn two aggregates that share the store
    let proxy_a = AggregateProps::<Counter>::new(AggregateId::new("share-a"), Arc::clone(&store))
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    let proxy_b = AggregateProps::<Counter>::new(AggregateId::new("share-b"), Arc::clone(&store))
        .with_codec(Arc::new(PassThroughCodec))
        .spawn(&system)
        .await;

    proxy_a.ask(Increment).await.expect("a 1 must succeed");
    proxy_b.ask(Increment).await.expect("b 1 must succeed");
    proxy_a.ask(Increment).await.expect("a 2 must succeed");

    // Then: each stream has the expected count, independently
    assert_eq!(
        proxy_a.exec(GetCount).await.expect("exec a"),
        2,
        "stream a must show 2 increments"
    );
    assert_eq!(
        proxy_b.exec(GetCount).await.expect("exec b"),
        1,
        "stream b must show 1 increment"
    );
}
