//! Handles whose clones are another name for the one instance.
//!
//! Everything here is stated as what a caller can observe through the public
//! entry points: whether a dispatch is accepted, which sequences a stream ends
//! up holding, which process a handle names, and which events a subscriber
//! received. The *shape* of the sharing — that no holder wraps a store, a
//! supplier, a registry, a binding or a gate in an `Arc` of its own — is a
//! structural condition and is fixed by the issue's own greps rather than by a
//! test, because a type that shared correctly through an outer `Arc` would pass
//! every clause below.
//!
//! ```gherkin
//!   Scenario: EventSourceSystem's clone is the shared inner
//!     Given a system built on a store the caller still holds
//!     When the system is cloned and both resolve and decide the same id
//!     Then one activation numbers both appends, and the store the caller holds
//!          is where they landed
//!
//!   Scenario: the erased handle shares the snapshot persistor
//!     Given a system over a store that holds a snapshot slot
//!     When the persistor is taken, an aggregate is started through the erased
//!          handle, and the persistor is taken again
//!     Then every answer names the one persistor process
//!
//!   Scenario: hosted spawn reaches one activation
//!     When spawn_aggregate, aggregate_proxy and the erased handle's own
//!          spawn_aggregate each dispatch to the same id
//!     Then one writer numbers the three appends
//!
//!   Scenario: the registry is carried by value
//!     Given two clones of a system and the handle it erases to
//!     When all three race to resolve one identity and then all dispatch
//!     Then one activation accepted every append
//!
//!   Scenario: a reference's clone shares its binding
//!     When a pinned reference and a resolved one are each cloned
//!     Then the pinned pair address the one activation, and the resolved pair
//!          share the cache a death empties: the next dispatch through the
//!          other clone reaches a live activation first time
//!
//!   Scenario: a durable stream has one gate
//!     Given a stream spawned over a journal holding events, with no subscriber
//!     When several poll intervals pass and a subscriber then subscribes
//!     Then nothing was published before the subscribe, and the catchup is
//!          still there afterwards
//! ```
//!
//! What makes a second registry, a second supplier or a second activation
//! observable at all is the store's optimistic concurrency rule, exactly as in
//! `aggregate_resolve.rs`: two activations of one stream each carry their own
//! sequence counter, so the one that fell behind has its append rejected. A
//! clause that ends with every dispatch accepted and a gap-free sequence has
//! therefore seen exactly one writer, whichever handle reached it.
//!
//! Run with: `cargo test -p nitinol-eventsource --test cheap_clone_handles`.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Barrier, Notify};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::system::{EventSourceSystem, SystemHandle};
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, DurableStream, Event,
    EventEnvelope, Query, SequenceCursor,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::provider::{PersistenceProvider, Store};
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, Props, Receive};
use nitinol_runtime::ProcessSystem;

// Fixtures: the aggregate the resolve clauses dispatch to

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("cheap_clone"), TypeName::new("Incremented"));

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
    type Rejection = Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = Infallible;

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

// Helpers

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &Journal, id: &AggregateId) -> Vec<u64> {
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
/// addresses next is otherwise decided by scheduling. An activation answers user
/// messages only after that replay, so one query round-trip both waits for it
/// and reports where it landed.
async fn pin_replayed_state(proxy: &AggregateProxy<Counter>, expected: u64) {
    let count = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        count, expected,
        "the activation must be parked after {expected} event(s) before the clause proceeds"
    );
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

// Scenario: EventSourceSystem's clone is the shared inner

/// A system and its clone are one instance: one registry, one supplier, one
/// data region.
///
/// The third dispatch is the discriminator for the registry half. Had the clone
/// resolved through a registry of its own it would have activated a second
/// `Counter`, replayed the one stored event and taken sequence 2 — leaving the
/// original's activation still holding sequence 1 as its next append, so the
/// last dispatch would come back as a rejection instead of succeeding.
///
/// The last clause is the half that cannot see: a clone reading through its own
/// supplier would answer with its own writes as readily as the original would,
/// so the store handle this clause kept is what settles whether the region was
/// shared or duplicated.
#[tokio::test]
async fn a_system_clone_shares_the_inner_instance() {
    // Given: a system built on a store this clause keeps hold of, and a clone
    let (system, store) = counter_system().await;
    let twin = system.clone();
    let id = AggregateId::new("cheap-clone-system-inner");

    // When: each resolves the same identity and dispatches through it
    let through_original = system.spawn_aggregate::<Counter>(id.clone()).await;
    through_original
        .ask(Increment)
        .await
        .expect("the first dispatch must persist");

    let through_twin = twin.spawn_aggregate::<Counter>(id.clone()).await;
    through_twin
        .ask(Increment)
        .await
        .expect("the clone's reference must dispatch to the same aggregate");

    through_original
        .ask(Increment)
        .await
        .expect("the original's reference must still be writing the same stream");

    // Then: one writer numbered every append, and the caller's own store handle
    // is where they landed
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1, 2, 3],
        "a clone names the same instance: one activation numbers the appends, \
         and the store the system was built on is the region they are in"
    );
    assert_eq!(
        through_twin
            .exec(GetCount)
            .await
            .expect("the query must reach the activation"),
        3,
        "one activation must have applied all three increments"
    );
}

// Scenario: the erased handle shares the snapshot persistor

/// One persistor process per system, across the erasure as well as across
/// clones.
///
/// The process identifier is the clause: the snapshot contract is written for a
/// single consumer serialising its calls, and a second process over one store
/// would be two consumers — which a round trip through the store cannot see,
/// because both would pass it.
///
/// Taking the persistor *before* the erasure is what makes the order matter: a
/// handle that answered from a cell of its own would leave the system it was
/// erased from and the handle it produced disagreeing about which process is
/// the one.
#[tokio::test]
async fn the_erased_handle_shares_the_snapshot_persistor() {
    // Given: a system over a store that holds a snapshot slot
    let (system, _store) = counter_system().await;
    let id = AggregateId::new("cheap-clone-erased-persistor");

    // When: the persistor is taken, an aggregate is started through the erased
    // handle, and the persistor is taken again
    let before = system.snapshot_persistor().await;

    let erased: SystemHandle = system.erase_store();
    erased
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("the erased handle's own entry point must persist");

    let after = system.snapshot_persistor().await;
    let through_clone = system.clone().snapshot_persistor().await;

    // Then
    assert_eq!(
        before.pid(),
        after.pid(),
        "the persistor a system distributes is one process: passing through the \
         erased handle must not leave a second one behind it"
    );
    assert_eq!(
        before.pid(),
        through_clone.pid(),
        "a clone distributes that same process rather than starting one of its \
         own"
    );
}

// Scenario: hosted spawn reaches one activation

/// Every hosted entry point reaches the one activation, including the erased
/// handle's own.
///
/// The three entry points below all go through the erasure internally — that is
/// what carries the registry to the props — and the last one erases a handle
/// that was already erased. A step that rebuilt the system's shared state
/// rather than naming it would put one activation per entry point on this
/// stream, and the store's concurrency rule would reject all but one of their
/// appends.
#[tokio::test]
async fn hosted_entry_points_reach_one_activation_through_the_erased_handle() {
    // Given
    let (system, store) = counter_system().await;
    let id = AggregateId::new("cheap-clone-hosted-entry-points");

    // When: the awaited entry point, the synchronous one, and the erased
    // handle's own each dispatch to the same identity
    system
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("spawn_aggregate must persist");

    system
        .aggregate_proxy::<Counter>(id.clone())
        .ask(Increment)
        .await
        .expect("a synchronously built reference must dispatch to the same aggregate");

    system
        .erase_store()
        .spawn_aggregate::<Counter>(id.clone())
        .await
        .ask(Increment)
        .await
        .expect("the erased handle must dispatch to the same aggregate");

    // Then
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1, 2, 3],
        "every hosted entry point resolves through the one registry the system \
         carries, so one writer numbers the three appends"
    );
}

// Scenario: the registry is carried by value

/// The number of callers that race for the same key: two clones of the typed
/// system and the handle it erases to.
const RACERS: usize = 3;

/// Resolve `id` on `system` in lockstep with the other racers, then dispatch.
///
/// Every racer resolves *and finishes replaying* before any of them dispatches,
/// so a registry that was not shared would leave `RACERS` writers all parked on
/// the genesis sequence and the store would reject all but one of their appends.
async fn race_to_resolve<St: Store>(
    system: EventSourceSystem<St>,
    id: AggregateId,
    barrier: Arc<Barrier>,
) -> (u64, Result<(), AskError<Infallible>>) {
    barrier.wait().await;
    let proxy = system.spawn_aggregate::<Counter>(id).await;
    // Drain the replay while the stream is still empty, so whatever this call
    // activated is parked on the genesis sequence.
    let replayed = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    barrier.wait().await;
    (replayed, proxy.ask(Increment).await)
}

/// Two clones and the erased handle race one identity to one activation.
///
/// The registry is the system's own state rather than something a holder shares
/// by wrapping it: a clone carries it, and so does the handle the store type is
/// erased from. Both axes are raced here, because an erasure that built a
/// registry of its own would still pass a clause that only cloned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_clones_race_one_identity_to_one_activation() {
    // Given
    let (system, store) = counter_system().await;
    let id = AggregateId::new("cheap-clone-registry-race");
    let barrier = Arc::new(Barrier::new(RACERS));

    // When
    let mut racers = Vec::with_capacity(RACERS);
    for _ in 0..RACERS - 1 {
        let system = system.clone();
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        racers.push(tokio::spawn(async move {
            race_to_resolve(system, id, barrier).await
        }));
    }
    let erased = system.erase_store();
    let erased_id = id.clone();
    let erased_barrier = Arc::clone(&barrier);
    racers.push(tokio::spawn(async move {
        race_to_resolve(erased, erased_id, erased_barrier).await
    }));

    // Then
    for (index, racer) in racers.into_iter().enumerate() {
        let (replayed, outcome) = tokio::time::timeout(Duration::from_secs(5), racer)
            .await
            .unwrap_or_else(|_| panic!("racer {index} must not wait forever for an activation"))
            .expect("racer task must not panic");

        assert_eq!(
            replayed, 0,
            "racer {index} must have reached an empty stream before any append"
        );
        if let Err(e) = outcome {
            panic!(
                "racer {index} must reach the one activation, but its append was rejected: {e:?}"
            );
        }
    }

    let expected: Vec<u64> = (1..=RACERS as u64).collect();
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        expected,
        "one writer must number every racer's append without a gap or a \
         collision: the registry a clone and an erased handle carry is the one \
         the system holds"
    );
}

// Scenario: a reference's clone shares its binding

/// A reference and its clone address the same thing, pinned or resolved.
///
/// The pinned half is the simpler statement: the clone carries the activation
/// the explicit lifecycle started, so both halves write one stream and one
/// aggregate state answers for both.
///
/// The resolved half is where the cache lives, and the last dispatch is the
/// discriminator. The twin's query is what put the activation in the cache; the
/// original then loses the stream to a rival and empties the cache on the
/// conflict it sees. A reference whose clone kept a cache of its own would send
/// the next dispatch to the activation that is already on its way out and have
/// it fail, where a shared cache costs that dispatch a resolve and nothing more.
#[tokio::test]
async fn a_reference_clone_shares_its_binding() {
    // Given
    let (system, store) = counter_system().await;
    let journal = store.events();

    // When: a pinned reference is cloned and both halves dispatch
    let pinned_id = AggregateId::new("cheap-clone-binding-pinned");
    let pinned = AggregateProps::<Counter>::new(pinned_id.clone(), journal.clone())
        .spawn(system.process_system())
        .await;
    let pinned_twin = pinned.clone();
    pinned
        .ask(Increment)
        .await
        .expect("the pinned reference's dispatch must persist");
    pinned_twin
        .ask(Increment)
        .await
        .expect("the clone must reach the activation the lifecycle started");

    // Then
    assert_eq!(
        stored_sequences(&journal, &pinned_id).await,
        vec![1, 2],
        "a pinned reference's clone carries the activation it was pinned to, so \
         one writer numbers both appends"
    );
    assert_eq!(
        pinned_twin
            .exec(GetCount)
            .await
            .expect("the query must reach the activation"),
        2,
        "one activation must have applied both increments"
    );

    // And: a resolved reference's clone shares the resolver and the cache
    let resolved_id = AggregateId::new("cheap-clone-binding-resolved");
    let rival = AggregateProps::<Counter>::new(resolved_id.clone(), journal.clone())
        .spawn(system.process_system())
        .await;
    rival
        .ask(Increment)
        .await
        .expect("the rival's genesis append");

    let resolved = system.aggregate_proxy::<Counter>(resolved_id.clone());
    let resolved_twin = resolved.clone();
    // The twin's dispatch is what activates and caches; the original has made
    // none of its own.
    pin_replayed_state(&resolved_twin, 1).await;

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
            AskError::Persist(PersistError::Append(AppendError::SequenceConflict(_)))
        ),
        "the store's OCC rejection must reach the caller unchanged, got {err:?}"
    );

    // The very next dispatch through the other clone — one call, no retry loop
    tokio::time::timeout(Duration::from_secs(5), resolved_twin.ask(Increment))
        .await
        .expect("the single next dispatch must not hang")
        .expect(
            "the death the other clone observed must have emptied the cache this \
             one reads, so this dispatch reaches the re-activated aggregate",
        );

    assert_eq!(
        resolved_twin
            .exec(GetCount)
            .await
            .expect("the query must reach the re-activated aggregate"),
        3,
        "the re-activated aggregate must have replayed both of the rival's \
         events and applied the healed dispatch's own on top of them"
    );
    assert_eq!(
        stored_sequences(&journal, &resolved_id).await,
        vec![1, 2, 3],
        "healing must continue the stream rather than re-write a taken sequence"
    );
}

// Scenario: a durable stream has one gate

#[derive(Clone)]
struct Evt;

/// The codec `Evt` names. This clause appends payloads by hand and turns loaded
/// events into envelopes itself, so the format only has to exist.
struct UnitCodec;

impl Codec<Evt> for UnitCodec {
    type Error = Infallible;

    fn encode(_: &Evt) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<Evt, Self::Error> {
        Ok(Evt)
    }
}

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::new(Family::new("cheap_clone"), TypeName::new("Evt"));

    type Codec = UnitCodec;
}

fn to_envelope(loaded: LoadedEvent) -> Option<EventEnvelope<Evt>> {
    if loaded.event_type != Evt::EVENT_TYPE {
        return None;
    }
    Some(EventEnvelope {
        aggregate_id: AggregateId::new(loaded.stream_key),
        sequence: loaded.sequence,
        global_sequence: loaded.global_sequence,
        event: Evt,
    })
}

/// Captures every envelope it receives in publish order.
struct RecordingSubscriber {
    sequences: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Process for RecordingSubscriber {}

impl Receive<EventEnvelope<Evt>> for RecordingSubscriber {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<Evt>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Self::Error> {
        self.sequences
            .lock()
            .expect("the recorder's lock guards a push and returns, so no holder panics")
            .push(msg.sequence);
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

struct Recorder {
    sequences: Arc<Mutex<Vec<u64>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            sequences: Arc::new(Mutex::new(Vec::new())),
            count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn producer(&self) -> impl Fn() -> RecordingSubscriber + Send + Sync + 'static {
        let sequences = Arc::clone(&self.sequences);
        let count = Arc::clone(&self.count);
        let notify = Arc::clone(&self.notify);
        move || RecordingSubscriber {
            sequences: Arc::clone(&sequences),
            count: Arc::clone(&count),
            notify: Arc::clone(&notify),
        }
    }

    fn seen_sequences(&self) -> Vec<u64> {
        self.sequences
            .lock()
            .expect("the recorder's lock guards a read and returns, so no holder panics")
            .clone()
    }

    fn current_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    async fn wait_for_count(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notified = self.notify.notified();
                if self.current_count() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {expected} envelopes (received {})",
                self.current_count()
            )
        });
    }
}

const TEST_POLL_INTERVAL: Duration = Duration::from_millis(50);

async fn append_evt(store: &InMemoryEventStore, key: &str, sequence: u64) {
    store
        .append(
            key,
            vec![AppendingEvent {
                sequence,
                event_type: Evt::EVENT_TYPE,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

/// The poller publishes nothing until a subscribe opens the one gate it and the
/// handle share.
///
/// What a lost gate costs is the catchup: a poller publishing into a stream
/// nobody has subscribed to advances its cursor over events it dropped, so the
/// subscriber that arrives afterwards reads an empty stream rather than the
/// three rows that were there all along. The quiet stretch is the condition
/// under test — several poll intervals in which the poller would have consumed
/// them had the gate not held it — so the clause waits it out rather than
/// synchronising around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_durable_stream_opens_its_one_gate_on_subscribe() {
    // Given: a stream over a journal that already holds three rows, and a
    // subscriber process that has not subscribed
    let system = ProcessSystem::new().await;
    let store = InMemoryEventStore::default();
    let key = AggregateId::new("cheap-clone-gate-agg");
    for sequence in 1..=3 {
        append_evt(&store, key.as_str(), sequence).await;
    }

    let recorder = Recorder::new();
    let subscriber = system.spawn(Props::new(recorder.producer())).await;

    let stream = DurableStream::<EventEnvelope<Evt>>::new(
        ProcessName::new("cheap-clone-gate-topic"),
        store.clone(),
        to_envelope,
    )
    .cursor(SequenceCursor::Stream {
        key: key.as_str().to_owned(),
        after: 0,
    })
    .with_poll_interval(TEST_POLL_INTERVAL)
    .spawn(&system)
    .await
    .expect("DurableStream::spawn must succeed");

    // When: several poll intervals pass with the gate closed, and a subscriber
    // then subscribes
    tokio::time::sleep(TEST_POLL_INTERVAL * 5).await;
    assert_eq!(
        recorder.seen_sequences(),
        Vec::<u64>::new(),
        "nothing may reach a subscriber that has not subscribed"
    );

    stream
        .subscribe(subscriber)
        .await
        .expect("subscribe must succeed");

    // Then
    recorder.wait_for_count(3).await;
    assert_eq!(
        recorder.seen_sequences(),
        vec![1, 2, 3],
        "the gate the poller reads and the one `subscribe` opens are one: a \
         poller that published before the subscribe would have advanced its \
         cursor past these rows and left nothing to deliver"
    );
}
