// What an aggregate activation does with a `Decision` (L-2 to L-5).
//
// A decision is a value: it states the facts that follow from a command and the
// answer the caller asked for, or it states a refusal.  The clauses below are
// what the activation interpreting it must do, and each one is observed only
// through the public reference — what `ask` answers with, how many times the
// store was asked to append, and what the stream ends up holding.
//
// L-2: the facts of one acceptance are persisted as a single atomic append.
// L-3: an acceptance with no facts appends nothing and still answers.
// L-4: a refusal is accompanied by no persistence whatsoever.
// L-5: `ask` answers with the decision's output; `tell` discards the output but
//      still surfaces a refusal rather than letting it vanish, and a refusal
//      does not take the activation down with it.
//
// The store here counts the appends it is asked for, because "one append" and
// "three appends that happen to land in order" leave the same stream behind and
// are otherwise indistinguishable from the outside.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Query,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("decision"), TypeName::new("Incremented"));
}

// Fixtures: aggregate

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

// Fixtures: commands and query

/// Accepts unconditionally, producing one fact.
struct Increment;

/// Accepts unconditionally, producing as many facts as it names, in one
/// decision.
struct IncrementBy(u64);

/// Accepts either way: a counter already at the floor has nothing left to do,
/// which is an acceptance with no facts rather than a refusal.
struct EnsureAtLeast(u64);

/// Refused once the counter has reached the limit.
struct IncrementIfLessThan(u64);

struct GetCount;

#[derive(Debug, thiserror::Error)]
#[error("counter already at {0}")]
struct AtMaxError(u64);

impl Decider<Increment> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Decider<IncrementBy> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: IncrementBy) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented; cmd.0 as usize]).output(self.value + cmd.0)
    }
}

impl Decider<EnsureAtLeast> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: EnsureAtLeast) -> Decision<Incremented, u64, Self::Rejection> {
        if self.value >= cmd.0 {
            return Decision::persist(Vec::new()).output(self.value);
        }
        Decision::persist(vec![Incremented; (cmd.0 - self.value) as usize]).output(cmd.0)
    }
}

impl Decider<IncrementIfLessThan> for Counter {
    type Output = u64;
    type Rejection = AtMaxError;

    fn decide(&self, cmd: IncrementIfLessThan) -> Decision<Incremented, u64, AtMaxError> {
        if self.value >= cmd.0 {
            return Decision::reject(AtMaxError(self.value));
        }
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
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

// Fixtures: a store that reports how often it was asked to append

/// Delegates every call to the in-memory reference store and counts the
/// `append` calls, so that "one append carrying three events" can be told from
/// "three appends carrying one event each".
struct CountingStore {
    inner: InMemoryEventStore,
    appends: Arc<AtomicUsize>,
}

/// How many times `store` has been asked to append since it was created.
struct AppendCount(Arc<AtomicUsize>);

impl AppendCount {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

fn counting_store() -> (Arc<dyn EventStore>, AppendCount) {
    let appends = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn EventStore> = Arc::new(CountingStore {
        inner: InMemoryEventStore::default(),
        appends: Arc::clone(&appends),
    });
    (store, AppendCount(appends))
}

#[async_trait]
impl EventStore for CountingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Helpers

/// Activate one writer for `id` on `store`.
async fn spawn_counter(
    ps: &ProcessSystem,
    id: &AggregateId,
    store: &Arc<dyn EventStore>,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id.clone(), Arc::clone(store))
        .with_codec(Arc::new(JsonCodec))
        .spawn(ps)
        .await
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

// L-5: the ask path answers with the output

/// `ask` answers with the output the decision states, not with the facts it
/// produced.
///
/// Both asks persist exactly one event, so an interpreter that answered with
/// the events — or with how many of them there were — would answer `1` twice.
/// The output counts the state the command reached, so it must be `1` and then
/// `2`.
#[tokio::test]
async fn ask_answers_with_the_output_of_the_decision() {
    // Given
    let ps = ProcessSystem::new().await;
    let (store, _appends) = counting_store();
    let id = AggregateId::new("decision-ask-output");
    let proxy = spawn_counter(&ps, &id, &store).await;

    // When
    let first = proxy.ask(Increment).await.expect("the first ask");
    let second = proxy.ask(Increment).await.expect("the second ask");

    // Then
    assert_eq!(
        (first, second),
        (1, 2),
        "each ask must answer with the output its decision stated"
    );
}

// L-2: one acceptance is one append

/// The facts of a single acceptance reach the store as one atomic append.
///
/// Appending them one at a time would leave the same three sequences behind,
/// which is why the count of appends is asserted as well: a reader must never
/// be able to observe the second fact of a decision without the first.
#[tokio::test]
async fn accepted_facts_are_persisted_as_a_single_append() {
    // Given
    let ps = ProcessSystem::new().await;
    let (store, appends) = counting_store();
    let id = AggregateId::new("decision-atomic-append");
    let proxy = spawn_counter(&ps, &id, &store).await;

    // When
    proxy
        .ask(IncrementBy(3))
        .await
        .expect("a decision carrying three facts must be accepted");

    // Then
    assert_eq!(
        appends.get(),
        1,
        "the three facts of one decision must be committed as one unit"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "the facts must be numbered consecutively from the sequence the writer replayed to"
    );
}

// L-3: an acceptance with no facts

/// An acceptance that produced no facts appends nothing and still answers.
///
/// This is how a command that finds its work already done stays idempotent, so
/// it must not be turned into a refusal, and it must not reach the store at all
/// — an empty append is still a call the store has to arbitrate.
#[tokio::test]
async fn empty_acceptance_answers_without_reaching_the_store() {
    // Given: the counter is already at the floor the command asks for
    let ps = ProcessSystem::new().await;
    let (store, appends) = counting_store();
    let id = AggregateId::new("decision-empty-acceptance");
    let proxy = spawn_counter(&ps, &id, &store).await;
    proxy
        .ask(IncrementBy(2))
        .await
        .expect("the counter must reach the floor first");
    let appends_before = appends.get();

    // When
    let output = proxy
        .ask(EnsureAtLeast(2))
        .await
        .expect("an acceptance with no facts is an acceptance");

    // Then
    assert_eq!(
        output, 2,
        "the answer must be delivered as usual even though nothing was appended"
    );
    assert_eq!(
        appends.get(),
        appends_before,
        "an acceptance with no facts must not ask the store for anything"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2],
        "the stream must be left exactly as the earlier command wrote it"
    );
}

// L-4: a refusal writes nothing

/// A refused command is answered as a rejection and leaves no trace.
#[tokio::test]
async fn refusal_is_answered_as_a_rejection_and_writes_nothing() {
    // Given
    let ps = ProcessSystem::new().await;
    let (store, appends) = counting_store();
    let id = AggregateId::new("decision-refusal");
    let proxy = spawn_counter(&ps, &id, &store).await;

    // When
    let err = proxy
        .ask(IncrementIfLessThan(0))
        .await
        .expect_err("the decider must refuse at the limit");

    // Then
    assert!(
        matches!(err, AskError::Rejection(AtMaxError(0))),
        "the refusal must reach the caller as the decider's own rejection value, got {err:?}"
    );
    assert_eq!(
        appends.get(),
        0,
        "a refusal must not ask the store for anything"
    );
    assert!(
        stored_sequences(&store, &id).await.is_empty(),
        "a refusal is a statement about a command, so it must leave no fact behind"
    );
}

// L-5: the tell path discards the output but not the aggregate

/// A refusal on the tell path is not a failure of the activation.
///
/// Nobody is waiting for the answer, so the refusal has nowhere to be returned
/// — but it is a verdict on one command, not a fault of the aggregate, and an
/// activation that stopped on it would take every later command down with a
/// business rule.
#[tokio::test]
async fn refusal_on_the_tell_path_leaves_the_activation_answering() {
    // Given
    let ps = ProcessSystem::new().await;
    let (store, appends) = counting_store();
    let id = AggregateId::new("decision-tell-refusal");
    let proxy = spawn_counter(&ps, &id, &store).await;

    // When: the queue is FIFO, so the query is answered by the same activation
    // after it has handled the refused command.
    proxy
        .tell(IncrementIfLessThan(0))
        .await
        .expect("the command must be accepted for delivery");

    // Then
    let count = proxy
        .exec(GetCount)
        .await
        .expect("a refused command must not stop the activation that refused it");
    assert_eq!(count, 0, "a refused command must change nothing");
    assert_eq!(
        appends.get(),
        0,
        "a refusal must not ask the store for anything on the tell path either"
    );
}
