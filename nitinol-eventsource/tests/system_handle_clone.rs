//! `EventSourceSystem` as a shared handle.
//!
//! Cloning an `EventSourceSystem` produces another name for the same instance:
//! every clone resolves through the same aggregate registry and carries the same
//! persistence supplier.  That is what lets callers drop the outer
//! `Arc<EventSourceSystem<..>>` they used to need in order to hand the system to
//! more than one task.
//!
//! ```gherkin
//!   Scenario: a clone writes where the store the system was built on reads
//!     Given a system built on a store the caller still holds
//!     When an aggregate is resolved and told to decide through a clone
//!     Then the handle the caller holds is where its events landed
//! ```
//!
//! What makes a second registry observable is the store's optimistic
//! concurrency rule, exactly as in `aggregate_resolve.rs`: two activations of
//! one stream each carry their own sequence counter, so the one that fell behind
//! has its append rejected.  A test that ends with every dispatch accepted and a
//! gap-free sequence has therefore seen exactly one writer, whichever clone
//! reached it.
//!
//! What makes a second data region observable is the store handle this file
//! constructed. A clone reading through its own supplier would pass that clause
//! against a copy of the store as readily as against the store itself, so the
//! last clause reads the handle the caller kept instead.
//!
//! Single-flight across clones under concurrency is pinned by
//! `aggregate_resolve.rs::concurrent_resolve_produces_a_single_writer`, which
//! races its resolvers from separate clones.  The tests here are the sequential
//! statement of the same sharing, plus the type-level part that has no runtime
//! observation at all.

use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::{EventSourceSystem, EventSourceSystemBuilder, PersistenceUnset};
use nitinol_eventsource::{codec::Codec, Aggregate, Decider, Decision, Event, Query};
use nitinol_persistence::provider::{PersistenceProvider, Store};
use nitinol_persistence::store::{EventStore, InMemoryStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("handle"), TypeName::new("Incremented"));

    type Codec = JsonCodec;
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
async fn stored_sequences(store: &dyn EventStore, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.iter().map(|event| event.sequence).collect()
}

/// A system on a fresh in-memory store, plus the store handle to read it back
/// from.
async fn counter_system() -> (EventSourceSystem<InMemoryStore>, InMemoryStore) {
    let ps = ProcessSystem::new().await;
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    (system, store)
}

// Type-level contracts

fn assert_clone<T: Clone>() {}

/// The system type is the shared handle, whether or not a store was supplied.
///
/// It carries no codec type at all — each event names its own — so the store is
/// the only axis this has to be stated across.
#[allow(dead_code)]
fn _event_source_system_is_clone_in_both_supplier_states() {
    assert_clone::<EventSourceSystem<PersistenceUnset>>();
    assert_clone::<EventSourceSystem<InMemoryStore>>();
}

/// `builder(ps)` is the name of the entry point that returns a builder.
#[allow(dead_code)]
async fn _builder_is_the_entry_point() {
    let ps = ProcessSystem::new().await;
    let _: EventSourceSystemBuilder<PersistenceUnset> = EventSourceSystem::builder(ps);
}

// Sharing across clones

/// Clones resolve one identity to one activation: there is one aggregate
/// registry behind them all.
///
/// The third dispatch is the discriminator.  Had the clone resolved through a
/// registry of its own, it would have activated a second `Counter`, replayed the
/// one stored event, and taken sequence 2 — leaving the original's activation
/// still holding sequence 1 as its next append, so this last dispatch would come
/// back as a rejection instead of succeeding.
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
        stored_sequences(&store.events(), &id).await,
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

/// Clones carry the supplier the system was built with.
///
/// Read back through the *clone's* supplier rather than the store this clause
/// kept: a clone that dropped the supplier would either fail to build props at
/// all or serve them from an empty store, and reading through the original would
/// hide both.
#[tokio::test]
async fn clone_and_original_share_the_suppliers_event_store() {
    // Given: a system and a clone of it
    let (system, _store) = counter_system().await;
    let handle = system.clone();
    let id = AggregateId::new("clone-shared-store");

    // When: an event is persisted through the original's supplier
    system
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("the dispatch must persist");

    // Then: the clone's supplier answers with the store that received it
    assert_eq!(
        stored_sequences(&*handle.persistence().event_store(), &id).await,
        vec![1],
        "the clone must carry the supplier the original was built with, \
         not an empty store of its own"
    );
}

/// A clone writes into the data region the store handle the caller holds names.
///
/// This is the half the clause above cannot see. A clone that copied the store
/// into a region of its own would still read its own writes back through its own
/// supplier, so the handle this clause constructed is what settles whether the
/// region was shared or duplicated.
#[tokio::test]
async fn a_clone_writes_where_the_store_the_system_was_built_on_reads() {
    // Given: a system built on a store this clause keeps hold of, and a clone
    let (system, store) = counter_system().await;
    let handle = system.clone();
    let id = AggregateId::new("clone-store-handle-identity");

    // When: the dispatch goes through the clone
    handle
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("the clone's dispatch must persist");

    // Then: the handle the caller holds is where it landed
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1],
        "a system's clone distributes the store the system was built on, so a \
         caller holding that store reads what the clone wrote"
    );
}
