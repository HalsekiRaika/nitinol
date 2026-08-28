// Resolve-layer contracts of EventSourceSystem (R-1, R-3, R-5).
//
// Everything asserted here is stated in terms of what a caller can observe
// through the public entry points: whether a dispatch is accepted, and which
// sequences the aggregate's stream ends up holding.  Nothing distinguishes
// "newly activated" from "joined a live activation" — that indistinguishability
// is contract R-3, so the tests never ask which one happened.
//
// The store's OCC rule is what makes a second activation observable at all: two
// activations of one stream each carry their own sequence counter, so the one
// that fell behind has its append rejected.  A test that ends with every
// dispatch accepted and a gap-free sequence has therefore seen exactly one
// writer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Barrier;

use nitinol_eventsource::error::EffectExecutionError;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Context, Decider, Effect,
    Event, Query, SnapshotPersistor, Snapshotable,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("resolve"), TypeName::new("Incremented"));
}

// Fixtures: aggregates

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

/// Snapshot-capable twin of [`Counter`], used only to reach
/// `AggregateProps::with_snapshot_persistor`.
#[derive(Default)]
struct SnapshottingCounter {
    value: u64,
}

impl Aggregate for SnapshottingCounter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for SnapshottingCounter {
    type Snapshot = u64;

    fn capture(&self) -> u64 {
        self.value
    }

    fn restore(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

// Fixtures: command and query

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

#[async_trait]
impl Decider<Increment> for SnapshottingCounter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

// Fixtures: codec

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

// Helpers

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &Arc<dyn EventStore>, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.iter().map(|event| event.sequence).collect()
}

/// Wait until the activation behind `proxy` has finished replaying, and confirm
/// it stopped at `expected` events.
///
/// A spawn returns before `on_start` has replayed, so which sequence a writer
/// addresses next is otherwise decided by scheduling.  An activation answers
/// user messages only after that replay, so one query round-trip both waits for
/// it and reports where it landed.
async fn pin_replayed_state(proxy: &AggregateProxy<Counter>, expected: u64) {
    let count = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        count, expected,
        "the activation must be parked after {expected} event(s) before the test proceeds"
    );
}

// R-1: concurrent resolve of one key

/// The number of callers that race for the same key.  Any value above one
/// exercises the contract; sixteen keeps the losing side of a broken
/// implementation unambiguous.
const CONCURRENT_RESOLVERS: usize = 16;

/// R-1: concurrent resolves of one key must not produce more than one activation.
///
/// Every task resolves *and finishes replaying* before any task dispatches, so
/// an implementation that activated per call would leave `CONCURRENT_RESOLVERS`
/// writers all parked on the genesis sequence, and the store would reject all
/// but one of their appends.  A single-flight registry leaves one writer, which
/// numbers the appends 1..=CONCURRENT_RESOLVERS.
///
/// Each task races from its own clone of the system rather than from a shared
/// reference, so this pins single-flight *across handles* as well: cloning names
/// the same instance, and a clone that carried an aggregate registry of its own
/// would put one activation per task on the stream — precisely the state the
/// paragraph above describes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_resolve_produces_a_single_writer() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("resolve-single-flight");
    let barrier = Arc::new(Barrier::new(CONCURRENT_RESOLVERS));

    // When: all callers resolve, then all callers dispatch
    let mut resolvers = Vec::with_capacity(CONCURRENT_RESOLVERS);
    for _ in 0..CONCURRENT_RESOLVERS {
        let system = system.clone();
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        resolvers.push(tokio::spawn(async move {
            barrier.wait().await;
            let proxy = system.spawn_aggregate::<Counter>(id).await;
            // Drain the replay while the stream is still empty, so whatever this
            // call activated is parked on the genesis sequence.
            let replayed = proxy
                .exec(GetCount)
                .await
                .expect("the activation must answer once it has replayed");
            barrier.wait().await;
            (replayed, proxy.ask(Increment).await)
        }));
    }

    // Then
    for (index, resolver) in resolvers.into_iter().enumerate() {
        let (replayed, outcome) = tokio::time::timeout(Duration::from_secs(5), resolver)
            .await
            .unwrap_or_else(|_| panic!("resolver {index} must not wait forever for an activation"))
            .expect("resolver task must not panic");

        assert_eq!(
            replayed, 0,
            "resolver {index} must have reached an empty stream before any append"
        );

        match outcome {
            Ok(events) => assert_eq!(
                events,
                vec![Incremented],
                "resolver {index} must persist exactly one event"
            ),
            Err(e) => panic!(
                "resolver {index} must reach the one activation, but its append was rejected: {e:?}"
            ),
        }
    }

    let expected: Vec<u64> = (1..=CONCURRENT_RESOLVERS as u64).collect();
    assert_eq!(
        stored_sequences(&store, &id).await,
        expected,
        "one writer must number every resolver's append without a gap or a collision"
    );
}

// R-3 / SPAWN-AGG: resolving twice addresses one aggregate

/// R-3: a second `spawn_aggregate` for the same id returns a reference to the
/// same aggregate as the first.
///
/// The third dispatch is the discriminator: two activations would each hold
/// their own sequence counter, so `first` would still be at sequence 1 after
/// `second` wrote sequence 2, and its append would collide.
#[tokio::test]
async fn second_spawn_aggregate_addresses_the_same_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("resolve-second-spawn");

    let first = system.spawn_aggregate::<Counter>(id.clone()).await;
    first.ask(Increment).await.expect("first dispatch");

    // When
    let second = system.spawn_aggregate::<Counter>(id.clone()).await;
    second
        .ask(Increment)
        .await
        .expect("the second reference must dispatch to the same aggregate");
    first
        .ask(Increment)
        .await
        .expect("the first reference must still be writing the same stream");

    // Then
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "both references must feed one writer, which numbers the appends 1, 2, 3"
    );
    assert_eq!(
        second.exec(GetCount).await.expect("exec must succeed"),
        3,
        "one activation must have applied all three increments"
    );
}

// PROPS-AGG: the props path resolves through the same registry

/// `aggregate_props(..).spawn(..)` must reach the same aggregate as
/// `spawn_aggregate`, not a second activation of it.
#[tokio::test]
async fn aggregate_props_spawn_addresses_the_same_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("resolve-props-path");

    let spawned = system.spawn_aggregate::<Counter>(id.clone()).await;
    spawned.ask(Increment).await.expect("first dispatch");

    // When
    let from_props = system
        .aggregate_props::<Counter>(id.clone())
        .spawn(system.process_system())
        .await;
    from_props
        .ask(Increment)
        .await
        .expect("the props path must dispatch to the same aggregate");
    spawned
        .ask(Increment)
        .await
        .expect("the original reference must still be writing the same stream");

    // Then
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "both entry points must feed one writer"
    );
}

/// The snapshot-persistor step of the props builder must not drop the resolve
/// wiring: `with_snapshot_persistor` rebuilds the builder, and a field lost
/// there would silently send this one entry point back to per-call activation.
#[tokio::test]
async fn aggregate_props_with_snapshot_persistor_addresses_the_same_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let snapshot_ref = SnapshotPersistor::spawn(
        system.process_system(),
        Arc::new(InMemorySnapshotStore::default()),
    )
    .await;
    let id = AggregateId::new("resolve-props-snapshot");

    let first = system
        .aggregate_props::<SnapshottingCounter>(id.clone())
        .with_snapshot_persistor(snapshot_ref.clone(), system.codec::<u64>())
        .spawn(system.process_system())
        .await;
    first.ask(Increment).await.expect("first dispatch");

    // When
    let second = system
        .aggregate_props::<SnapshottingCounter>(id.clone())
        .with_snapshot_persistor(snapshot_ref, system.codec::<u64>())
        .spawn(system.process_system())
        .await;
    second
        .ask(Increment)
        .await
        .expect("a snapshot-configured props must dispatch to the same aggregate");
    first
        .ask(Increment)
        .await
        .expect("the first reference must still be writing the same stream");

    // Then
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "configuring a snapshot persistor must not restore per-call activation"
    );
}

// R-5: a reference outlives the activation it is pointing at

/// R-5: a reference stays valid across the death of the activation it reached.
///
/// A rival writer is spawned through the raw `AggregateProps` path, which keeps
/// activating per call, so it can take the stream away from the resolved
/// reference's activation.  The resolved activation then loses an append on a
/// non-genesis sequence, stops because of it, and the *very next* dispatch through the
/// same reference — no retry loop — must reach a re-activated aggregate that
/// replayed everything the rival wrote: the proxy evicts the dying activation
/// as soon as it sees the conflict, not only after a later send fails against it.
#[tokio::test]
async fn reference_survives_the_death_of_its_activation() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("resolve-self-heal");

    let rival = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
        .with_codec(system.codec::<Incremented>())
        .spawn(system.process_system())
        .await;
    rival
        .ask(Increment)
        .await
        .expect("the rival's genesis append");

    // Resolved after sequence 1 exists, so this activation replays to 1 and its
    // next append is not a genesis append.
    let resolved = system.spawn_aggregate::<Counter>(id.clone()).await;
    pin_replayed_state(&resolved, 1).await;

    rival
        .ask(Increment)
        .await
        .expect("the rival takes sequence 2");

    let err = resolved
        .ask(Increment)
        .await
        .expect_err("an activation that is no longer the only writer must be rejected");
    assert!(
        matches!(
            err,
            AskError::Effect(EffectExecutionError::Append(AppendError::SequenceConflict(
                _
            )))
        ),
        "the store's OCC rejection must reach the caller unchanged, got {err:?}"
    );

    // When: the very next dispatch through the same reference — a single call,
    // no retry loop — must reach the re-activated aggregate.
    let events = tokio::time::timeout(Duration::from_secs(5), resolved.ask(Increment))
        .await
        .expect("the single next dispatch must not hang")
        .expect("the next dispatch must reach the re-activated aggregate, not the dying one");

    // Then
    assert_eq!(
        events,
        vec![Incremented],
        "the healed dispatch must persist its event like any other"
    );
    assert_eq!(
        resolved
            .exec(GetCount)
            .await
            .expect("the query must reach the re-activated aggregate"),
        3,
        "the re-activated aggregate must have replayed both events the rival wrote"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "healing must continue the stream rather than re-write a taken sequence"
    );
}
