//! Props carry *what* to start, and the system they are spawned on supplies
//! every store and persistor that start needs.
//!
//! ```gherkin
//!   Scenario: props started from a system are handed no store
//!     Given a system built on PersistenceProvider::new(InMemoryStore::default())
//!     When AggregateProps::<Counter>::new(id).spawn(&system) runs a command
//!     Then the events are readable through system.persistence().event_store()
//!
//!   Scenario: both entry points reach one activation
//!     Given one system
//!     When AggregateProps::<Counter>::new(id).spawn(&system) and
//!          system.aggregate_props::<Counter>(id).spawn() are called in turn
//!     Then both references resolve to the same activation, read off the
//!          store's optimistic concurrency rule: the appends dispatched through
//!          the two references in turn are all accepted, the sequence carries no
//!          gap, and one query sees every one of them
//!     And no entry point for taking a pid is added to AggregateProxy
//!
//!   Scenario: a Latest aggregate recovers with nothing declared at the wiring
//!     Given Counter is Snapshotable and declares `Latest` as its recovery
//!     When it runs on a system, a snapshot is saved through
//!          system.snapshot_persistor(), later events are appended, and the
//!          aggregate is started again on a second system over the same store
//!     Then the restarted aggregate recovers from the snapshot and replays only
//!          the events written after it
//!     And the persistor that system distributes is the one process it was
//!         before the activation recovered through it
//!
//!   Scenario: a Never aggregate spawns on any store
//!     Given a system over a CustomStore that holds only a journal
//!     When AggregateProps::<Order>::new(id).spawn(&system) runs
//!     Then it compiles and starts
//!
//!   Scenario: both durable-stream entry points read the system's journal
//!     Given a system, and a stream started from each entry point
//!     When an aggregate of that system persists an event
//!     Then the event reaches both streams' subscribers
//! ```
//!
//! What every clause reads is *the store the system distributes*, so props that
//! reached a store of their own would hand back a working handle and leave
//! nothing behind where this file looks.
//!
//! "The same activation" is what the store's optimistic concurrency rule
//! answers: two activations of one stream each count their own sequence, so the
//! overtaken one has its append rejected. A run whose three dispatches are all
//! accepted, numbered without a gap, and all visible to one query has had
//! exactly one writer. There is no pid beside that to compare, because an
//! aggregate reference names an aggregate rather than a process.
//!
//! The snapshot clause writes a value no replay could produce — a snapshot at
//! sequence 5 holding 1000 over a stream of ones — so a start that ignored the
//! snapshot and a start that ignored the events after it are both a different
//! number from the one asserted.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test props_take_their_stores_from_the_system`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::TryStreamExt;
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, Decider, Decision, DurableStream, Event, Latest, Never, Query,
    SequenceCursor, Snapshotable,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Store};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, EventType, Family, LoadQuery, LoadedEvent, PersistedSnapshot, TypeName,
};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, Props, Receive};
use nitinol_runtime::ProcessSystem;

// Fixtures: events

/// Pass-through codec: `Incremented` carries no payload, so the format only has
/// to exist.
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

#[derive(Clone, Debug, PartialEq)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("props.from.system"),
        TypeName::new("Incremented"),
    );

    type Codec = EventCodec;
}

#[derive(Clone, Debug, PartialEq)]
struct Placed;

impl Codec<Placed> for EventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Placed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Placed, Self::Error> {
        Ok(Placed)
    }
}

impl Event for Placed {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("props.from.system"), TypeName::new("Placed"));

    type Codec = EventCodec;
}

// Fixtures: a snapshot-recovering aggregate

/// Big-endian `u64` codec for the counter's snapshot value.
struct SnapshotCodec;

#[derive(Debug, thiserror::Error)]
#[error("a counter snapshot payload is 8 bytes, got {0}")]
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

// Fixtures: an aggregate that never recovers from a snapshot

/// Stateless: what the clause reads is the journal the system distributes, so
/// the aggregate needs no state of its own to answer with.
#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = Placed;
    type Recover = Never;

    fn apply(&mut self, _event: Placed) {}
}

struct PlaceOrder;

impl Decider<PlaceOrder> for Order {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: PlaceOrder) -> Decision<Placed, (), Self::Rejection> {
        Decision::persist(vec![Placed]).output(())
    }
}

// Fixtures: a durable-stream subscriber

/// What a loaded row is carried to a subscriber as.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    sequence: u64,
}

/// Carry every `Incremented` row and skip the rest, advancing the cursor past
/// what it skipped.
fn to_seen(loaded: LoadedEvent) -> Option<Seen> {
    (loaded.event_type == Incremented::EVENT_TYPE).then_some(Seen {
        sequence: loaded.sequence,
    })
}

/// Records what it was published, so a clause can wait for an arrival rather
/// than for a duration.
struct Subscriber {
    seen: Arc<Mutex<Vec<Seen>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Process for Subscriber {}

impl Receive<Seen> for Subscriber {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: Seen,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Self::Error> {
        self.seen.lock().expect(SEEN_LOCK).push(msg);
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

/// Why the subscriber's log lock cannot be poisoned: `recv` pushes and returns,
/// and never panics while holding it.
const SEEN_LOCK: &str = "the subscriber's log is only held across a push, so no holder panics";

/// The subscriber side of one stream, kept so a clause can read what arrived.
struct Arrivals {
    seen: Arc<Mutex<Vec<Seen>>>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Arrivals {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
            count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn producer(&self) -> impl Fn() -> Subscriber + Send + Sync + 'static {
        let seen = Arc::clone(&self.seen);
        let count = Arc::clone(&self.count);
        let notify = Arc::clone(&self.notify);
        move || Subscriber {
            seen: Arc::clone(&seen),
            count: Arc::clone(&count),
            notify: Arc::clone(&notify),
        }
    }

    /// Wait until `want` payloads have arrived, then answer all of them.
    async fn wait_for(&self, want: usize, label: &str) -> Vec<Seen> {
        tokio::time::timeout(PROBE_TIMEOUT, async {
            loop {
                let notified = self.notify.notified();
                if self.count.load(Ordering::SeqCst) >= want {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label}: {} payloads arrived, wanted {want}",
                self.count.load(Ordering::SeqCst)
            )
        });
        self.seen.lock().expect(SEEN_LOCK).clone()
    }
}

// Helpers

/// How long a clause waits for work a process performs on its own loop.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Kept short so the two streams' catch-up is not the slowest part of a run.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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

/// A system over a store that holds every slot.
async fn in_memory_system(store: InMemoryStore) -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

// Scenario: props started from a system are handed no store

/// Props built with nothing but an identity write to the journal the system
/// they were spawned on distributes.
#[tokio::test]
async fn props_spawned_on_a_system_write_to_the_store_it_distributes() {
    // Given
    let system = in_memory_system(InMemoryStore::default()).await;
    let id = AggregateId::new("props-from-system-counter");

    // When: the props name no store, no provider and no persistor
    let counter = AggregateProps::<Counter>::new(id.clone())
        .spawn(&system)
        .await;
    counter
        .ask(Increment)
        .await
        .expect("the command must be accepted");

    // Then: the journal the system distributes is where it landed
    assert_eq!(
        stored_sequences(&system.persistence().event_store(), &id).await,
        vec![1],
        "props spawned on a system take their journal from it, so the event is \
         in the store that system distributes rather than in one the caller named"
    );
}

// Scenario: both entry points reach one activation

/// The props-first and the system-first entry points resolve one activation of
/// one identity.
#[tokio::test]
async fn both_aggregate_entry_points_reach_one_activation() {
    // Given
    let store = InMemoryStore::default();
    let system = in_memory_system(store.clone()).await;
    let id = AggregateId::new("props-from-system-one-activation");

    // When: the two entry points are called in turn for the same identity
    let via_props = AggregateProps::<Counter>::new(id.clone())
        .spawn(&system)
        .await;
    via_props
        .ask(Increment)
        .await
        .expect("the first dispatch must persist");

    let via_system = system.aggregate_props::<Counter>(id.clone()).spawn().await;
    via_system
        .ask(Increment)
        .await
        .expect("the second entry point must dispatch to the same aggregate");

    via_props
        .ask(Increment)
        .await
        .expect("the first reference must still be writing the same stream");

    // Then: one writer numbered every append, and one state saw them all
    assert_eq!(
        stored_sequences(&store.events(), &id).await,
        vec![1, 2, 3],
        "both entry points resolve the identity through the one registry the \
         system owns; a second activation of this stream would have had its \
         append rejected and cost one of these numbers"
    );
    assert_eq!(
        via_system
            .exec(GetCount)
            .await
            .expect("the query must reach the activation"),
        3,
        "one activation must have applied all three increments"
    );
}

// Scenario: a Latest aggregate recovers with nothing declared at the wiring

/// An aggregate whose type says `Recover = Latest` recovers from the snapshot
/// the system's own persistor holds, then replays only the events after it.
#[tokio::test]
async fn a_latest_aggregate_recovers_through_the_systems_snapshot_persistor() {
    // Given: five events on a store, and a snapshot of a state no replay of
    // them could reach
    let store = InMemoryStore::default();
    let system = in_memory_system(store.clone()).await;
    let id = AggregateId::new("props-from-system-recover");

    let counter = AggregateProps::<Counter>::new(id.clone())
        .spawn(&system)
        .await;
    for _ in 0..5 {
        counter
            .ask(Increment)
            .await
            .expect("each command must be accepted");
    }

    system
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 5,
            payload: Bytes::from(1000u64.to_be_bytes().to_vec()),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("saving a snapshot through the system's persistor succeeds");

    for _ in 0..2 {
        counter
            .ask(Increment)
            .await
            .expect("each command after the snapshot must be accepted");
    }

    // When: the aggregate is started again on a second system over the same
    // store, so the activation is a new one rather than the resident one
    let restarted = in_memory_system(store.clone()).await;
    let persistor_before = restarted.snapshot_persistor().await.pid();

    let revived = AggregateProps::<Counter>::new(id.clone())
        .spawn(&restarted)
        .await;

    // Then: the snapshot is where it started, and the two later events are all
    // it replayed
    assert_eq!(
        revived
            .exec(GetCount)
            .await
            .expect("the query must reach the restarted activation"),
        1002,
        "the aggregate's own type asked to recover from the latest snapshot, so \
         the state is the snapshot's 1000 plus the two events written after it \
         — a start that ignored the snapshot would hold 7, and one that ignored \
         those events would hold 1000"
    );

    // And: the persistor the system distributes is still the one process it was
    assert_eq!(
        restarted.snapshot_persistor().await.pid(),
        persistor_before,
        "a system distributes one snapshot persistor, so recovering an \
         activation through it leaves the same process answering"
    );
}

// Scenario: a Never aggregate spawns on any store

/// An aggregate whose type says `Recover = Never` starts on a store that holds
/// nothing but a journal.
#[tokio::test]
async fn a_never_aggregate_spawns_on_a_store_holding_only_a_journal() {
    // Given: a store with no snapshot slot to offer
    let journal_only = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .build();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(journal_only))
        .build();
    let id = AggregateId::new("props-from-system-never");

    // When
    let order = AggregateProps::<Order>::new(id.clone())
        .spawn(&system)
        .await;
    order
        .ask(PlaceOrder)
        .await
        .expect("the command must be accepted");

    // Then
    assert_eq!(
        stored_sequences(&system.persistence().event_store(), &id).await,
        vec![1],
        "an aggregate that never recovers from a snapshot asks its system for \
         nothing but the journal, so a store holding only one is enough to run it"
    );
}

// Scenario: both durable-stream entry points read the system's journal

/// A stream started from props and a stream taken from the system both follow
/// the journal that system distributes.
#[tokio::test]
async fn both_durable_stream_entry_points_read_the_systems_journal() {
    // Given: one subscriber per entry point
    let system = in_memory_system(InMemoryStore::default()).await;
    let id = AggregateId::new("props-from-system-stream-source");

    let from_props = Arrivals::new();
    let from_system = Arrivals::new();
    let props_subscriber = system
        .process_system()
        .spawn(Props::new(from_props.producer()))
        .await;
    let system_subscriber = system
        .process_system()
        .spawn(Props::new(from_system.producer()))
        .await;

    // When: one stream is built from props and one is taken from the system,
    // and neither is handed a journal
    let props_stream = DurableStream::<Seen>::new(ProcessName::new("stream-from-props"), to_seen)
        .cursor(SequenceCursor::Global { after: 0 })
        .with_poll_interval(POLL_INTERVAL)
        .spawn(&system)
        .await
        .expect("spawning a stream from props on a system succeeds");
    props_stream
        .subscribe(props_subscriber)
        .await
        .expect("subscribing to the props-built stream succeeds");

    let system_stream = system
        .durable_stream::<Seen, _>(ProcessName::new("stream-from-system"), to_seen)
        .cursor(SequenceCursor::Global { after: 0 })
        .with_poll_interval(POLL_INTERVAL)
        .spawn()
        .await
        .expect("spawning a stream taken from the system succeeds");
    system_stream
        .subscribe(system_subscriber)
        .await
        .expect("subscribing to the system-built stream succeeds");

    // And: an aggregate of that system persists one event
    let counter = system.spawn_aggregate::<Counter>(id).await;
    counter
        .ask(Increment)
        .await
        .expect("the command must be accepted");

    // Then: both subscribers were published that event
    assert_eq!(
        from_props
            .wait_for(1, "the props-built stream must read the system's journal")
            .await,
        vec![Seen { sequence: 1 }],
        "a stream spawned on a system polls the journal that system \
         distributes, so an event written through the system reaches it"
    );
    assert_eq!(
        from_system
            .wait_for(1, "the system-built stream must read the same journal")
            .await,
        vec![Seen { sequence: 1 }],
        "the entry point on the system reaches the same journal, so the two \
         forms are two ways to say one wiring"
    );
}
