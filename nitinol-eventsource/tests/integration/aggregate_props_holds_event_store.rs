//! `AggregateProps::new` receives an identity and nothing else, and the
//! aggregate process holds the journal its system distributes inline.
//!
//! ```gherkin
//!   Scenario: the entry point receives an identity alone
//!     Given a system over a journal the caller still holds
//!     When props are built on three different aggregate ids of that system
//!     Then all three compile
//!     And every aggregate they spawn appends into the one journal
//!
//!   Scenario: the system's journal is what a replay reads
//!     Given an aggregate that has decided twice
//!     When a second system over the same journal starts that id again
//!     Then it restores the state those decisions left
//! ```
//!
//! A deployment names its journal once, where it builds its system; whichever
//! form its wiring holds one in — a backend it constructed, a [`Journal`] a
//! supplier answered with — is that supplier's business and never the props'.
//! What the props carry is the identity, and the clause that matters is that
//! every aggregate of one system shares one *destination* too — props that
//! reached a store of their own would compile just as well and leave each
//! aggregate writing somewhere else.
//!
//! The process boundary this replaced is gone: the aggregate calls
//! `store.append` / `store.load` on the handle it holds rather than asking a
//! persistor process to. Replay is the clause that pins it end to end.

use bytes::Bytes;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Decider, Decision, Event, Never, Query,
};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

use futures_util::TryStreamExt;

// Minimal counter aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));

    type Codec = PassThroughCodec;
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;
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

/// How many events `journal` holds under `key`.
async fn appended_count(journal: &Journal, key: &AggregateId) -> usize {
    let loaded: Vec<_> = journal
        .load(LoadQuery::by_stream(key))
        .await
        .expect("loading a stream of a journal succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of a journal succeeds");
    loaded.len()
}

/// A system distributing `journal`, with a registry of its own.
async fn system_over(journal: &Journal) -> EventSourceSystem<Journal> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(journal.clone()))
        .build()
}

// Compile-time type assertion

/// `AggregateProps::<Counter>::new` MUST receive the identity and nothing else.
///
/// Not a `#[test]` — the compiler enforces the signature at build time. A
/// regression to a receiving shape that also takes a journal, a supplier or a
/// persistor is a compile error here.
#[allow(dead_code)]
fn _assert_sig_aggregate_props_new_receives_an_identity_alone() {
    let _: fn(AggregateId) -> AggregateProps<Counter> = AggregateProps::new;
}

// Runtime integration: every aggregate of one system reaches the one journal

/// Spawning an `AggregateProcess` succeeds for every identity a system is asked
/// for, and the facts of every accepted decision reach the one journal behind
/// them.
///
/// The three streams are read back through the handle the test kept, so an
/// aggregate that reached a destination of its own would leave its stream empty
/// here while its own `ask` still succeeded.
#[tokio::test]
async fn every_aggregate_of_one_system_reaches_the_one_journal() {
    // Given: one journal, named once, and a system distributing it
    let journal = Journal::from(InMemoryEventStore::default());
    let system = system_over(&journal).await;
    let first = AggregateId::new("journal-one-first");
    let second = AggregateId::new("journal-one-second");
    let third = AggregateId::new("journal-one-third");

    // When: props are built on each identity and each aggregate decides once
    for id in [first.clone(), second.clone(), third.clone()] {
        let proxy = AggregateProps::<Counter>::new(id.clone())
            .spawn(&system)
            .await;
        proxy
            .ask(Increment)
            .await
            .unwrap_or_else(|error| panic!("ask on {} must succeed: {error:?}", id.as_str()));
    }

    // Then: all three streams are in the journal the test holds
    assert_eq!(
        (
            appended_count(&journal, &first).await,
            appended_count(&journal, &second).await,
            appended_count(&journal, &third).await,
        ),
        (1, 1, 1),
        "props name an identity and take their journal from the system, so each \
         aggregate's fact is in the journal the test kept — props that reached a \
         store of their own would each have a region and nothing would report it"
    );
}

// Runtime integration: state replay via the system's journal

/// After two decisions on the first activation, a second system over the same
/// journal replays the stored events in `on_start` and observes the restored
/// state.
///
/// Two systems are what make this two activations: one system resolves an
/// identity to one activation, so a second start of the id on it would reach
/// the activation that is already there and replay nothing.
#[tokio::test]
async fn an_aggregate_replays_its_state_through_a_clone_of_the_journal() {
    // Given
    let journal = Journal::from(InMemoryEventStore::default());
    let system = system_over(&journal).await;
    let id = AggregateId::new("direct-store-replay");

    // Write 2 events through the first activation
    {
        let proxy = AggregateProps::<Counter>::new(id.clone())
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask 1 must succeed");
        proxy.ask(Increment).await.expect("ask 2 must succeed");
    }

    // When: a fresh system over the same journal starts the same id
    let restarted = system_over(&journal).await;
    let replayed = AggregateProps::<Counter>::new(id).spawn(&restarted).await;

    // Then
    assert_eq!(
        replayed.exec(GetCount).await.expect("exec must succeed"),
        2,
        "replay reads the journal the first activation appended to, so the \
         restored state is 2"
    );
}

// Several aggregates over one journal

/// Two `AggregateProcess` instances on different ids run over clones of the one
/// journal their system distributes.
///
/// Without a persistor process in between, the store itself is the
/// synchronisation point — its own thread-safe internals are what make that
/// sound, which is why identity is the backend's obligation rather than
/// something the framework establishes by sharing a pointer.
#[tokio::test]
async fn several_aggregate_processes_run_over_clones_of_one_journal() {
    // Given
    let journal = Journal::from(InMemoryEventStore::default());
    let system = system_over(&journal).await;

    // When: two aggregates are spawned on the one system
    let proxy_a = AggregateProps::<Counter>::new(AggregateId::new("share-a"))
        .spawn(&system)
        .await;

    let proxy_b = AggregateProps::<Counter>::new(AggregateId::new("share-b"))
        .spawn(&system)
        .await;

    proxy_a.ask(Increment).await.expect("a 1 must succeed");
    proxy_b.ask(Increment).await.expect("b 1 must succeed");
    proxy_a.ask(Increment).await.expect("a 2 must succeed");

    // Then: each stream shows its own count, independently
    assert_eq!(
        (
            proxy_a.exec(GetCount).await.expect("exec a"),
            proxy_b.exec(GetCount).await.expect("exec b"),
        ),
        (2, 1),
        "clones of one journal are one journal, and the streams within it stay \
         their own"
    );
}
