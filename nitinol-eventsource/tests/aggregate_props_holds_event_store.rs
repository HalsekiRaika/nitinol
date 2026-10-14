//! `AggregateProps::new` receives the journal as `impl Into<Journal>`, and the
//! aggregate process holds it inline.
//!
//! ```gherkin
//!   Scenario: the entry point receives a journal however the caller holds one
//!     Given a backend handle, a Journal, and a reference to that Journal
//!     When props are built on each of the three
//!     Then all three compile
//!     And every aggregate they spawn appends into the one journal
//!
//!   Scenario: the held journal is what a replay reads
//!     Given an aggregate that has decided twice
//!     When a second process for the same id is spawned on the same journal
//!     Then it restores the state those decisions left
//! ```
//!
//! A deployment holds its journal in whichever of the three forms its wiring
//! produced: a backend it constructed, a [`Journal`] a supplier answered with,
//! or a reference to one it has to keep. `impl Into<Journal>` is what makes all
//! three the same call, and the clause that matters is that they are the same
//! *destination* too — a receiving shape that silently erased a second time, or
//! took a copy of the backend's shape, would compile just as well and leave each
//! form writing somewhere else.
//!
//! The process boundary this replaced is gone: the aggregate calls
//! `store.append` / `store.load` on the handle it holds rather than asking a
//! persistor process to. Replay is the clause that pins it end to end.

use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Decider, Decision, Event, Query,
};
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

// Compile-time type assertion

/// `AggregateProps::<Counter>::new` MUST receive the journal as
/// `impl Into<Journal>`, which is these three call shapes.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time. A
/// regression to a handle the framework shares for the caller, or to a
/// receiving shape that takes only one of the three forms, is a compile error
/// here.
#[allow(dead_code)]
fn _assert_sig_aggregate_props_new_receives_any_form_of_a_journal<'a>(_held: &'a Journal) {
    let _: fn(AggregateId, InMemoryEventStore) -> AggregateProps<Counter> = AggregateProps::new;
    let _: fn(AggregateId, Journal) -> AggregateProps<Counter> = AggregateProps::new;
    let _: fn(AggregateId, &'a Journal) -> AggregateProps<Counter> = AggregateProps::new;
}

// Runtime integration: every receiving form reaches the one journal

/// Spawning an `AggregateProcess` succeeds whichever form the journal arrives
/// in, and the facts of every accepted decision reach the one journal behind
/// them.
///
/// The three streams are read back through the handle the test kept, so a form
/// that erased into a destination of its own would leave its stream empty here
/// while its own `ask` still succeeded.
#[tokio::test]
async fn every_form_of_a_journal_reaches_the_one_journal() {
    // Given: one backend, and the three forms a caller can hold it in
    let system = ProcessSystem::new().await;
    let backend = InMemoryEventStore::default();
    let journal = Journal::from(backend.clone());
    let through_backend = AggregateId::new("journal-form-backend");
    let through_journal = AggregateId::new("journal-form-journal");
    let through_reference = AggregateId::new("journal-form-reference");

    // When: props are built on each form and each aggregate decides once
    for (id, props) in [
        (
            through_backend.clone(),
            AggregateProps::<Counter>::new(through_backend.clone(), backend.clone()),
        ),
        (
            through_journal.clone(),
            AggregateProps::<Counter>::new(through_journal.clone(), journal.clone()),
        ),
        (
            through_reference.clone(),
            AggregateProps::<Counter>::new(through_reference.clone(), &journal),
        ),
    ] {
        let proxy = props.spawn(&system).await;
        proxy
            .ask(Increment)
            .await
            .unwrap_or_else(|error| panic!("ask on {} must succeed: {error:?}", id.as_str()));
    }

    // Then: all three streams are in the journal the test holds
    assert_eq!(
        (
            appended_count(&journal, &through_backend).await,
            appended_count(&journal, &through_journal).await,
            appended_count(&journal, &through_reference).await,
        ),
        (1, 1, 1),
        "a backend, a journal and a reference to that journal are three ways of \
         naming one destination, so each aggregate's fact is in the journal the \
         test kept — a form that erased a second time would have its own region \
         and nothing would report it"
    );
}

// Runtime integration: state replay via the held journal

/// After two decisions on process 1, a second process for the same id over the
/// same journal replays the stored events in `on_start` and observes the
/// restored state.
///
/// This is the key behaviour the refactor preserves: replay still works, and the
/// handle the second process holds is a clone of the first's rather than a
/// shared pointer to it.
#[tokio::test]
async fn an_aggregate_replays_its_state_through_a_clone_of_the_journal() {
    // Given
    let system = ProcessSystem::new().await;
    let journal = Journal::from(InMemoryEventStore::default());
    let id = AggregateId::new("direct-store-replay");

    // Write 2 events through process 1
    {
        let proxy = AggregateProps::<Counter>::new(id.clone(), &journal)
            .spawn(&system)
            .await;
        proxy.ask(Increment).await.expect("ask 1 must succeed");
        proxy.ask(Increment).await.expect("ask 2 must succeed");
    }

    // When: a fresh process for the same id is spawned on the same journal
    let replayed = AggregateProps::<Counter>::new(id, journal.clone())
        .spawn(&system)
        .await;

    // Then
    assert_eq!(
        replayed.exec(GetCount).await.expect("exec must succeed"),
        2,
        "replay reads the journal the first process appended to, so the restored \
         state is 2"
    );
}

// Several processes over one journal

/// Two `AggregateProcess` instances on different ids run over clones of one
/// journal.
///
/// Without a persistor process in between, the store itself is the
/// synchronisation point — its own thread-safe internals are what make that
/// sound, which is why identity is the backend's obligation rather than
/// something the framework establishes by sharing a pointer.
#[tokio::test]
async fn several_aggregate_processes_run_over_clones_of_one_journal() {
    // Given
    let system = ProcessSystem::new().await;
    let journal = Journal::from(InMemoryEventStore::default());

    // When: two aggregates are spawned on clones of the journal
    let proxy_a = AggregateProps::<Counter>::new(AggregateId::new("share-a"), journal.clone())
        .spawn(&system)
        .await;

    let proxy_b = AggregateProps::<Counter>::new(AggregateId::new("share-b"), journal.clone())
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
