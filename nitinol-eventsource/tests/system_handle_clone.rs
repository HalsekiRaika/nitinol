//! `EventSourceSystem` as a shared handle.
//!
//! Cloning an `EventSourceSystem` produces another name for the same instance:
//! every clone resolves through the same aggregate registry and carries the same
//! default [`EventStore`].  That is what lets callers drop the outer
//! `Arc<EventSourceSystem<..>>` they used to need in order to hand the system to
//! more than one task.
//!
//! What makes a second registry observable is the store's OCC rule, exactly as
//! in `aggregate_resolve.rs`: two activations of one stream each carry their own
//! sequence counter, so the one that fell behind has its append rejected.  A
//! test that ends with every dispatch accepted and a gap-free sequence has
//! therefore seen exactly one writer, whichever clone reached it.
//!
//! Single-flight across clones under concurrency is pinned by
//! `aggregate_resolve.rs::concurrent_resolve_produces_a_single_writer`, which
//! races its resolvers from separate clones.  The tests here are the sequential
//! statement of the same sharing, plus the type-level part that has no runtime
//! observation at all.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::{
    EventSourceSystem, EventSourceSystemBuilder, StoreSet, StoreUnset, Unset,
};
use nitinol_eventsource::{codec::Codec, Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("handle"), TypeName::new("Incremented"));
}

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

/// A system on a fresh in-memory store, plus the store to read it back from.
async fn counter_system() -> (EventSourceSystem<JsonCodec, StoreSet>, Arc<dyn EventStore>) {
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    (system, store)
}

// Type-level contracts

/// A codec marker that deliberately does not implement `Clone`.
///
/// The system holds `C` only as `PhantomData`, so a `C: Clone` requirement would
/// be an artefact of how the impl was written rather than anything the system
/// needs — and it would lock out every codec that is not itself `Clone`.
struct NotCloneCodec;

fn assert_clone<T: Clone>() {}

/// The system type is the shared handle, in both store states, without asking
/// anything of the codec type.
#[allow(dead_code)]
fn _event_source_system_is_clone_without_a_clone_codec() {
    assert_clone::<EventSourceSystem<NotCloneCodec, StoreUnset>>();
    assert_clone::<EventSourceSystem<NotCloneCodec, StoreSet>>();
}

/// `builder(ps)` is the name of the entry point that returns a builder.
#[allow(dead_code)]
async fn _builder_is_the_entry_point() {
    let ps = ProcessSystem::new().await;
    let _: EventSourceSystemBuilder<Unset, StoreUnset> = EventSourceSystem::builder(ps);
}

// Sharing across clones

/// Clones resolve one identity to one activation: there is one aggregate
/// registry behind them all.
///
/// The third dispatch is the discriminator.  Had the clone resolved through a
/// registry of its own, it would have activated a second `Counter`, replayed the
/// one stored event, and taken sequence 2 — leaving the original's activation
/// still holding sequence 1 as its next append, so this last dispatch would come
/// back as an OCC rejection instead of succeeding.
#[tokio::test]
async fn clone_and_original_resolve_through_one_aggregate_registry() {
    // Given: a system and a clone of it
    let (system, store) = counter_system().await;
    let handle = system.clone();
    let id = AggregateId::new("clone-shared-registry");

    // When: each resolves the same identity and dispatches through it
    let through_original = system.spawn_aggregate::<Counter>(id.clone()).await;
    through_original
        .ask(Increment)
        .await
        .expect("the first dispatch must persist");

    let through_clone = handle.spawn_aggregate::<Counter>(id.clone()).await;
    through_clone
        .ask(Increment)
        .await
        .expect("the clone's reference must dispatch to the same aggregate");

    through_original
        .ask(Increment)
        .await
        .expect("the original's reference must still be writing the same stream");

    // Then: one writer numbered every append, without a gap or a collision
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "both clones must feed one writer; a per-clone registry would put two \
         activations on this stream"
    );
    assert_eq!(
        through_clone
            .exec(GetCount)
            .await
            .expect("the query must reach the activation"),
        3,
        "one activation must have applied all three increments"
    );
}

/// Clones carry the default store the system was built with.
///
/// Read back through the *clone's* accessor rather than the store the test kept:
/// a clone that dropped the default would either fail to build props at all or
/// serve them from an empty store, and reading through the original would hide
/// both.
#[tokio::test]
async fn clone_and_original_share_the_default_event_store() {
    // Given: a system and a clone of it
    let (system, _store) = counter_system().await;
    let handle = system.clone();
    let id = AggregateId::new("clone-shared-store");

    // When: an event is persisted through the original's default store
    system
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("the dispatch must persist");

    // Then: the clone's default store is the one that received it
    assert_eq!(
        stored_sequences(handle.event_store(), &id).await,
        vec![1],
        "the clone must carry the default store the original was built with, \
         not an empty one of its own"
    );
}
