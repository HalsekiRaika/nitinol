//! Props taken from a system carry where they start, so spawning them needs
//! nothing re-injected.
//!
//! ```gherkin
//!   Scenario: an aggregate started from the system's own props
//!     Given props for an aggregate taken from a system
//!     When the caller spawns them without naming a process system
//!     Then the aggregate runs on the system the props came from
//!     And its events are in the store that system was built on
//!
//!   Scenario: the system's props resolve rather than activate
//!     Given an aggregate already resolved through a system
//!     When props taken from that system are spawned for the same identity
//!     Then one writer numbered every append
//!
//!   Scenario: a projector started from the system's own props
//!     Given props for a projector taken from a system
//!     When the caller spawns them without naming a process system
//!     Then it runs on the process system that system was built on
//!     And it applies the records in the store that system was built on
//!     And it records its position in that store's projection slot
//!
//!   Scenario: props built directly still name where they start
//!     Given props built from a store rather than from a system
//!     When the caller spawns them
//!     Then the process system they are given is where they start
//! ```
//!
//! What is observed is mostly the *store the system was built on*, read
//! through the handle this file constructed, rather than through anything the
//! system answers with. Props that reached a store of their own would hand
//! back a working proxy and leave nothing behind here. The projector scenario
//! is the exception: it also looks the returned proxy's pid up in the
//! system's own process system, because a projector proxy names a process
//! rather than an aggregate, so a process identifier is available to check
//! directly — unlike the aggregate scenario below, which has no such
//! identifier and so falls back to the resolve rule.
//!
//! "Runs on the system the props came from" is observed through the resolve
//! rule rather than through a process identifier, because an aggregate
//! reference names an aggregate rather than a process. Two activations of one
//! stream each carry a sequence counter of their own, so the one that fell
//! behind has its append rejected by the store's optimistic concurrency rule: a
//! run that ends with every dispatch accepted and a gap-free sequence has seen
//! exactly one writer. A hosted spawn that activated on a process system of its
//! own — or through a registry of its own — would be that second writer.
//!
//! The last clause is here because the low-level entry point is kept: props
//! built directly start a lifecycle on the process system they are handed, and
//! nothing about the hosted form takes that away.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, AggregateProps, Decider, Decision, Event,
    ProjectionContext, ProjectionId, Projector, Query,
};
use nitinol_persistence::provider::{
    CustomStore, PersistenceProvider, ProjectionOffsetSlot, Store,
};
use nitinol_persistence::store::{
    InMemoryEventStore, InMemoryOffsetStore, InMemoryStore, Journal, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("hosted.props"), TypeName::new("Incremented"));

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

struct CountingProjector {
    applied: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Incremented> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// Helpers

/// How long a clause waits for work a process performs on its own loop.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &Journal, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded.iter().map(|event| event.sequence).collect()
}

async fn wait_until_applied(
    applied: &Arc<AtomicUsize>,
    notify: &Arc<Notify>,
    want: usize,
    label: &str,
) -> usize {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        loop {
            let notified = notify.notified();
            let seen = applied.load(Ordering::SeqCst);
            if seen >= want {
                return seen;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{label}: the projector applied {} records, wanted at least {want}",
            applied.load(Ordering::SeqCst)
        )
    })
}

/// Wait until `store` holds a position for `consumer`, then answer it.
async fn wait_until_recorded(
    store: &dyn OffsetStore<Tx = ()>,
    consumer: &ProjectionId,
    label: &str,
) -> u64 {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        loop {
            if let Some(held) = store
                .load(&OffsetScope::from(consumer))
                .await
                .expect("loading an offset of an in-memory store succeeds")
            {
                return held;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}: no position was ever recorded"))
}

// Aggregate props

/// Props taken from a system spawn with no argument, and write to the store the
/// system was built on.
#[tokio::test]
async fn aggregate_props_from_a_system_spawn_without_a_process_system() {
    // Given
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let id = AggregateId::new("hosted-aggregate-props");

    // When: nothing but the props is named here
    let proxy = system.aggregate_props::<Counter>(id.clone()).spawn().await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1],
        "props taken from a system carry the store that system was built on, so \
         a spawn needing nothing re-injected still lands there"
    );
}

/// Props taken from a system resolve through that system rather than activating
/// a second time.
///
/// The third dispatch is the discriminator. Had the hosted spawn activated on a
/// process system or a registry of its own, it would have replayed the one
/// stored event and taken sequence 2 — leaving the first reference still
/// holding sequence 1 as its next append, so the last dispatch would come back
/// as a rejection instead of succeeding.
#[tokio::test]
async fn aggregate_props_from_a_system_resolve_on_that_system() {
    // Given: an aggregate already resolved through the system
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let id = AggregateId::new("hosted-aggregate-props-resolve");

    let first = system.spawn_aggregate::<Counter>(id.clone()).await;
    first
        .ask(Increment)
        .await
        .expect("the first dispatch must persist");

    // When: props taken from the system are spawned for the same identity
    let through_props = system.aggregate_props::<Counter>(id.clone()).spawn().await;
    through_props
        .ask(Increment)
        .await
        .expect("the hosted props must dispatch to the same aggregate");

    first
        .ask(Increment)
        .await
        .expect("the first reference must still be writing the same stream");

    // Then
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1, 2, 3],
        "a hosted spawn resolves through the system it came from; a second \
         activation of this stream would have cost one of these appends"
    );
    assert_eq!(
        through_props
            .exec(GetCount)
            .await
            .expect("the query must reach the activation"),
        3,
        "one activation must have applied all three increments"
    );
}

// Projector props

/// Props taken from a system spawn with no argument, follow the store the
/// system was built on, and record into that store's projection slot.
#[tokio::test]
async fn projector_props_from_a_system_spawn_without_a_process_system() {
    // Given: a system over a store this clause holds, with an aggregate that has
    // already decided twice
    let store = InMemoryStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build();
    let id = AggregateId::new("hosted-projector-props");
    let projection = ProjectionId::new("hosted-projector-props-read-model");

    let aggregate = system.spawn_aggregate::<Counter>(id.clone()).await;
    aggregate.ask(Increment).await.expect("ask must succeed");
    aggregate.ask(Increment).await.expect("ask must succeed");

    let applied = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let applied_for_producer = Arc::clone(&applied);
    let notify_for_producer = Arc::clone(&notify);

    // When: nothing but the props is named here
    let projector = system
        .projector_props(projection.clone(), move || CountingProjector {
            applied: Arc::clone(&applied_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_event::<Incremented>()
        .catchup_from_aggregate(id)
        .spawn()
        .await;

    // Then: the process the proxy names is one this system is running
    assert!(
        system
            .process_system()
            .lookup(projector.pid())
            .await
            .is_some(),
        "a projector spawned from a system runs on that system's process \
         system, so the system's own registry is where its process is found"
    );

    // And: it read the store the system was built on
    assert_eq!(
        wait_until_applied(
            &applied,
            &notify,
            2,
            "a hosted projector must catch up over the system's journal"
        )
        .await,
        2,
        "the projector follows the journal the system was built on, so it sees \
         both events the aggregate persisted there"
    );

    // And: it recorded into that store's projection slot
    let held = wait_until_recorded(
        &store.projection_offsets(),
        &projection,
        "a hosted projector must record into the system's projection slot",
    )
    .await;
    assert!(
        held >= 2,
        "the position recorded in the store's projection slot must have reached \
         the second event, and is {held}"
    );
}

// The entry point built directly

/// Props built from a store rather than from a system start on the process
/// system they are handed.
///
/// A lifecycle pinned to one aggregate is started this way, so the argument
/// stays where there is no system to take it from. What the hosted form
/// replaces is the call that took an argument and ignored it.
#[tokio::test]
async fn props_built_directly_start_on_the_process_system_they_are_given() {
    // Given
    let ps = ProcessSystem::new().await;
    let events = Journal::from(InMemoryEventStore::default());
    let id = AggregateId::new("unhosted-aggregate-props");

    // When
    let proxy = AggregateProps::<Counter>::new(id.clone(), &events)
        .spawn(&ps)
        .await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1],
        "props built directly keep the low-level form: the process system is \
         named at the spawn, and the store is the one they were built on"
    );
}

/// A system assembled from named handles reaches the same hosted spawn, so the
/// entry point is not tied to one store's shape.
#[tokio::test]
async fn hosted_props_work_over_an_assembled_store() {
    // Given
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .offsets(InMemoryOffsetStore::default())
            .build(),
    );
    let events = persistence.event_store();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(persistence)
        .build();
    let id = AggregateId::new("hosted-aggregate-props-assembled");

    // When
    let proxy = system.aggregate_props::<Counter>(id.clone()).spawn().await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1],
        "a hosted spawn over an assembled store lands in the journal handle that \
         store was named with"
    );
}
