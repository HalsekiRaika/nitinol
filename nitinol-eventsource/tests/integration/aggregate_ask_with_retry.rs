// `AggregateProxy::ask_with_retry`: an `ask` re-issued, a bounded number of
// times, while the error it answers is `Transient`.
//
// Every attempt is an ordinary `ask`. A writer that lost its stream has stopped
// and been dropped from the reference by the time the error comes back, so the
// next attempt reaches an activation that replays the stream first and decides
// the command again against what it finds there. Nothing here retries inside
// an activation: the one that lost still stops.
//
// How many times the command was tried is counted in the decider rather than
// at the store, because a refused command never reaches the store.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Never, Query, RetryPolicy,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ask-with-retry"), TypeName::new("Incremented"));

    type Codec = JsonCodec;
}

// Fixtures: aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: commands and query

/// How many times a command has been decided, shared by every clone of it.
#[derive(Clone, Default)]
struct Attempts(Arc<AtomicUsize>);

impl Attempts {
    fn record(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// The answer is the counter's new value, so a decision taken against state
/// that already holds another writer's event answers a higher number than one
/// taken against the state before it.
#[derive(Clone)]
struct Increment(Attempts);

/// Refused once the counter has reached the limit it carries.
#[derive(Clone)]
struct IncrementUntil {
    limit: u64,
    attempts: Attempts,
}

struct GetCount;

#[derive(Debug, thiserror::Error)]
#[error("counter already at {0}")]
struct AtLimit(u64);

impl Decider<Increment> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        cmd.0.record();
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Decider<IncrementUntil> for Counter {
    type Output = u64;
    type Rejection = AtLimit;

    fn decide(&self, cmd: IncrementUntil) -> Decision<Incremented, u64, AtLimit> {
        cmd.attempts.record();
        if self.value >= cmd.limit {
            return Decision::reject(AtLimit(self.value));
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

// Fixtures: stores

/// A store that has lost every append to another writer: each one is refused
/// as a sequence conflict, while what is already stored loads normally.
///
/// The stream is seeded with one event, so every activation replays to
/// sequence 1 and each refused append is a non-genesis one — the conflict an
/// activation stops on.
#[derive(Clone)]
struct OvertakenStore {
    inner: InMemoryEventStore,
}

impl OvertakenStore {
    async fn seeded(id: &AggregateId) -> Self {
        let inner = InMemoryEventStore::default();
        inner
            .append(
                id.as_str(),
                vec![AppendingEvent {
                    sequence: 1,
                    event_type: Incremented::EVENT_TYPE,
                    payload: <JsonCodec as Codec<Incremented>>::encode(&Incremented)
                        .expect("encoding the seed event must succeed"),
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("seeding the genesis event must succeed");
        Self { inner }
    }
}

#[async_trait]
impl EventStore for OvertakenStore {
    async fn append(
        &self,
        key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        Err(AppendError::SequenceConflict(key.to_owned()))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }

    async fn head(&self) -> Result<Option<u64>, LoadError> {
        self.inner.head().await
    }
}

/// A store whose first append fails in the backend and whose later appends go
/// through: a store that did not answer once, not one that refused the facts.
#[derive(Clone)]
struct FailFirstAppendStore {
    inner: InMemoryEventStore,
    fail_next_append: Arc<AtomicBool>,
}

impl FailFirstAppendStore {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            fail_next_append: Arc::new(AtomicBool::new(true)),
        }
    }
}

#[async_trait]
impl EventStore for FailFirstAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.fail_next_append.swap(false, Ordering::SeqCst) {
            return Err(AppendError::Backend(
                "simulated transient backend failure".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }

    async fn head(&self) -> Result<Option<u64>, LoadError> {
        self.inner.head().await
    }
}

// Helpers

/// A system distributing `store`, with an aggregate registry of its own.
///
/// One registry resolves an identity to one activation, so a second writer of
/// one stream is a second system over the same store.
async fn system_over(store: &Journal) -> EventSourceSystem<Journal> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build()
        .await
}

/// Activate one writer for `id` on `system`.
async fn spawn_writer(
    system: &EventSourceSystem<Journal>,
    id: &AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id.clone())
        .spawn(system)
        .await
}

/// Wait until `proxy`'s activation has finished replaying, and confirm it
/// stopped at `expected` events.
///
/// A spawn returns before the activation has replayed, and an activation
/// answers user messages only after that, so one query round-trip both waits
/// for it and pins which sequence the writer's next append addresses.
async fn pin_replayed_state(proxy: &AggregateProxy<Counter>, expected: u64) {
    let count = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        count, expected,
        "the writer must be parked after {expected} event(s) before the test proceeds"
    );
}

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

/// A policy whose waits are too short to matter to a test that is not about
/// them.
fn quick_policy(max_attempts: usize) -> RetryPolicy {
    RetryPolicy::new(max_attempts, Duration::from_millis(1), 2.0)
}

// Scenario: the writer that lost the race succeeds on a retry

/// Both writers are parked at sequence 1 before either asks, so whichever
/// appends second has decided against state that no longer holds: exactly one
/// of them loses sequence 2 to the other, whatever the scheduling.
///
/// The loser's retry reaches a fresh activation, which replays the winner's
/// event before deciding, so its answer is 3 rather than the 2 it decided the
/// first time.
#[tokio::test]
async fn the_writer_that_lost_the_race_succeeds_by_deciding_again() {
    // Given
    let store = Journal::from(InMemoryEventStore::default());
    let first_system = system_over(&store).await;
    let second_system = system_over(&store).await;
    let id = AggregateId::new("ask-with-retry-race");

    let first = spawn_writer(&first_system, &id).await;
    first
        .ask(Increment(Attempts::default()))
        .await
        .expect("genesis append");
    pin_replayed_state(&first, 1).await;

    let second = spawn_writer(&second_system, &id).await;
    pin_replayed_state(&second, 1).await;

    let first_attempts = Attempts::default();
    let second_attempts = Attempts::default();
    let policy = quick_policy(3);

    // When
    let (first_outcome, second_outcome) = tokio::join!(
        first.ask_with_retry(Increment(first_attempts.clone()), &policy),
        second.ask_with_retry(Increment(second_attempts.clone()), &policy),
    );

    // Then
    let first_output = first_outcome.expect("the first writer must end up with an answer");
    let second_output = second_outcome.expect("the second writer must end up with an answer");
    let mut tried = [
        (first_attempts.count(), first_output),
        (second_attempts.count(), second_output),
    ];
    tried.sort_unstable();
    assert_eq!(
        tried,
        [(1, 2), (2, 3)],
        "one writer must take sequence 2 on its first decision, and the other must lose it \
         once and then decide again against the winner's event, answering 3 — \
         (decisions, answer) per writer"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "both writers' events must be in the stream, in sequence order"
    );
}

// Scenario: the attempts run out

/// A store that refuses every append leaves every attempt a conflict, so the
/// policy's bound is the only thing that ends the retries, and the caller is
/// handed the conflict the last attempt received.
#[tokio::test]
async fn retries_stop_at_max_attempts_and_return_the_last_conflict() {
    // Given
    let id = AggregateId::new("ask-with-retry-exhausted");
    let store = Journal::from(OvertakenStore::seeded(&id).await);
    let system = system_over(&store).await;
    let writer = spawn_writer(&system, &id).await;
    pin_replayed_state(&writer, 1).await;
    let attempts = Attempts::default();

    // When
    let err = writer
        .ask_with_retry(Increment(attempts.clone()), &quick_policy(3))
        .await
        .expect_err("a command that loses every append must not be answered");

    // Then
    assert!(
        matches!(
            err,
            AskError::Persist(PersistError::Append(AppendError::SequenceConflict(_)))
        ),
        "the last attempt's conflict must reach the caller unchanged, got {err:?}"
    );
    assert_eq!(
        attempts.count(),
        3,
        "a policy of 3 attempts must decide the command exactly 3 times — the first try and \
         two retries"
    );
}

/// Between two attempts the caller waits what the policy states for the
/// attempt about to start, and nothing before the first or after the last.
///
/// The clock is paused, so time moves only while every task waits on a timer:
/// the elapsed time is the sum of the waits taken. With 40 ms and a multiplier
/// of 3, the two waits are 40 ms and 120 ms. A retry that does not wait, waits
/// the initial backoff every time, waits for the attempt just finished rather
/// than the one about to start, or also waits before the first or after the
/// last attempt, lands outside `[160 ms, 200 ms)`.
#[tokio::test(start_paused = true)]
async fn retries_wait_the_policys_backoff_before_each_further_attempt() {
    // Given
    let id = AggregateId::new("ask-with-retry-backoff");
    let store = Journal::from(OvertakenStore::seeded(&id).await);
    let system = system_over(&store).await;
    let writer = spawn_writer(&system, &id).await;
    pin_replayed_state(&writer, 1).await;
    let policy = RetryPolicy::new(3, Duration::from_millis(40), 3.0);

    // When
    let started = tokio::time::Instant::now();
    let outcome = writer
        .ask_with_retry(Increment(Attempts::default()), &policy)
        .await;
    let waited = started.elapsed();

    // Then
    assert!(
        outcome.is_err(),
        "the setup must exhaust the attempts, got {outcome:?}"
    );
    assert!(
        waited >= Duration::from_millis(160) && waited < Duration::from_millis(200),
        "the waits between 3 attempts must be backoff_before(2) + backoff_before(3) = \
         40 ms + 120 ms, got {waited:?}"
    );
}

// Scenario: a backend failure is transient too

/// What decides a retry is the error's retryability, not which error it is: a
/// store that failed to answer once is `Transient`, and the next attempt goes
/// through.
#[tokio::test]
async fn a_transient_backend_failure_is_retried() {
    // Given
    let store = Journal::from(FailFirstAppendStore::new());
    let system = system_over(&store).await;
    let id = AggregateId::new("ask-with-retry-backend");
    let writer = spawn_writer(&system, &id).await;
    let attempts = Attempts::default();

    // When
    let output = writer
        .ask_with_retry(Increment(attempts.clone()), &quick_policy(3))
        .await
        .expect("a store that answers on the second attempt must yield an answer");

    // Then
    assert_eq!(
        (attempts.count(), output),
        (2, 1),
        "the failed attempt must be retried once, and the retry must create the aggregate — \
         (decisions, answer)"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "only the retried append may have been stored"
    );
}

// Scenario: a permanent error is not retried

/// A decider's refusal is a verdict on the command, so the first one is the
/// answer.
#[tokio::test]
async fn a_rejection_is_returned_after_one_attempt() {
    // Given
    let store = Journal::from(InMemoryEventStore::default());
    let system = system_over(&store).await;
    let writer = spawn_writer(&system, &AggregateId::new("ask-with-retry-rejection")).await;
    let attempts = Attempts::default();

    // When
    let err = writer
        .ask_with_retry(
            IncrementUntil {
                limit: 0,
                attempts: attempts.clone(),
            },
            &quick_policy(3),
        )
        .await
        .expect_err("the decider must refuse at the limit");

    // Then
    assert!(
        matches!(err, AskError::Rejection(AtLimit(0))),
        "the refusal must reach the caller unchanged, got {err:?}"
    );
    assert_eq!(
        attempts.count(),
        1,
        "a permanent refusal must not be tried again"
    );
}

/// A creation that collides with an aggregate someone else created first would
/// collide again on every attempt, so the first collision is the answer.
#[tokio::test]
async fn already_created_is_returned_after_one_attempt() {
    // Given: both writers address the genesis sequence
    let store = Journal::from(InMemoryEventStore::default());
    let creating_system = system_over(&store).await;
    let redelivering_system = system_over(&store).await;
    let id = AggregateId::new("ask-with-retry-already-created");

    let creator = spawn_writer(&creating_system, &id).await;
    let redelivered = spawn_writer(&redelivering_system, &id).await;
    pin_replayed_state(&redelivered, 0).await;

    creator
        .ask(Increment(Attempts::default()))
        .await
        .expect("genesis append");
    let attempts = Attempts::default();

    // When
    let err = redelivered
        .ask_with_retry(Increment(attempts.clone()), &quick_policy(3))
        .await
        .expect_err("a creation that collides with an existing aggregate has no answer to give");

    // Then
    assert!(
        matches!(err, AskError::AlreadyCreated),
        "the collision must reach the caller unchanged, got {err:?}"
    );
    assert_eq!(
        attempts.count(),
        1,
        "a permanent collision must not be tried again"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "the stream must be left exactly as the creator wrote it"
    );
}
