use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Decider, Decision, Event, ProjectionContext,
    Projector, ProjectorProps,
};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, ProjectionId, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: minimal aggregate

#[derive(Default)]
struct Counter;

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));
}

impl Aggregate for Counter {
    type Event = Incremented;
    fn apply(&mut self, _event: Incremented) {}
}

struct Increment;

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

// Fixtures: minimal codec

struct IncrementedCodec;

impl Codec<Incremented> for IncrementedCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

// Fixtures: minimal projector

struct NoopProjector;

#[async_trait]
impl Projector<Incremented> for NoopProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Positive path: AggregateProps::new().with_codec().spawn()

/// The correct AggregateProps builder chain must compile and run successfully.
#[tokio::test]
async fn aggregate_props_correct_chain_compiles_and_spawns() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When: the full required chain
    let proxy = AggregateProps::<Counter>::new(AggregateId::new("typestate-agg-ok"), store)
        .with_codec(Arc::new(IncrementedCodec)) // transitions to CodecSet
        .spawn(&system) // only available in CodecSet state
        .await;

    // Then: spawn succeeds — proxy is usable
    proxy
        .ask(Increment)
        .await
        .expect("ask must succeed after correct typestate chain");
}

// Positive path: ProjectorProps full mandatory chain

/// The correct ProjectorProps builder chain must compile and run successfully.
#[tokio::test]
async fn projector_props_correct_chain_compiles_and_spawns() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    // When: full required chain
    let _proxy = ProjectorProps::new(
        ProjectionId::new("typestate-proj-ok"),
        store,
        checkpoint_store,
        NoopProjector::new,
    )
    .with_event::<Incremented>(Arc::new(IncrementedCodec)) // EventUnset → EventSet
    .catchup_from_global() // OriginUnset → OriginSet
    .spawn(&system) // only EventSet + OriginSet
    .await;

    // Then: spawn succeeds (no panic, no compile error)
}

impl NoopProjector {
    fn new() -> Self {
        NoopProjector
    }
}

// Positive path: multiple with_event() calls are allowed

/// Calling with_event() more than once must remain valid.
/// Registering multiple event types (EventSet → EventSet) must not be blocked.
#[tokio::test]
async fn projector_props_multiple_with_event_calls_compile_and_spawn() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    // When: two with_event() calls followed by catchup → spawn
    let _proxy = ProjectorProps::new(
        ProjectionId::new("typestate-multi-event-ok"),
        store,
        checkpoint_store,
        NoopProjector::new,
    )
    .with_event::<Incremented>(Arc::new(IncrementedCodec)) // first event type
    .with_event::<Incremented>(Arc::new(IncrementedCodec)) // second call: EventSet → EventSet
    .catchup_from_global()
    .spawn(&system)
    .await;

    // Then: spawn succeeds with multiple event registrations
}

// Positive path: catchup_from_aggregate variant

/// catchup_from_aggregate() must transition OriginUnset → OriginSet, enabling spawn().
#[tokio::test]
async fn projector_props_catchup_from_aggregate_enables_spawn() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());

    let agg_id = AggregateId::new("typestate-agg-origin");

    // When: using catchup_from_aggregate instead of catchup_from_global
    let _proxy = ProjectorProps::new(
        ProjectionId::new("typestate-proj-agg-origin"),
        store,
        checkpoint_store,
        NoopProjector::new,
    )
    .with_event::<Incremented>(Arc::new(IncrementedCodec))
    .catchup_from_aggregate(agg_id) // OriginUnset → OriginSet
    .spawn(&system)
    .await;

    // Then: spawn succeeds
}

// Compile-fail coverage
//
// The following scenarios must produce COMPILE ERRORS and are verified via
// rustdoc compile_fail doctests embedded in the AggregateProps::spawn and
// ProjectorProps::spawn rustdoc comments.
// See: nitinol-eventsource/src/process/props.rs and
//      nitinol-eventsource/src/projection/props.rs
