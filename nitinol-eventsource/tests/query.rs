//! Acceptance record for the eventsource query path: `AggregateProcess`
//! interprets `contract::Query<M>`.
//!
//! ```gherkin
//! Scenario: interpreting a query
//!   Given an AggregateProcess hosting an Aggregate that implements contract::Query<M>
//!   When a query message is sent with ask (AggregateProxy::exec)
//!   Then the query's Response comes back exactly once (L-5)
//!   And the query runs synchronously and purely, observing no Context (L-1)
//! ```
//!
//! The `Decider` fixtures below still take `&mut Context` — the decision path is
//! untouched here. The `Query` impls beside them take a message and nothing else.
//! That difference is the point: a question is answered from `&self` alone, so it
//! cannot reach the identity, the sequence, or an await point that a decision can.
//!
//! Replaces `tests/receive.rs`, which exercised the removed eventsource-resident
//! `Receive<M>` contract.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, Context, Decider, Effect, Event, ExecError, Query,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event and codec

/// A unit event representing one successful increment.
#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));
}

/// Pass-through codec: `Incremented` is a unit struct, so there is nothing to encode.
struct TestCodec;

impl Codec<Incremented> for TestCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

// Fixture: Counter — answers GetCount, refuses GetLabel until it has been incremented

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

/// Command: emits one `Incremented` event.
struct Increment;

/// Query: the current count.
struct GetCount;

/// Query: a human-readable label for the counter's position.
struct GetLabel;

/// The domain's own answer to a question it cannot answer — not a failure of the
/// machinery that carried the question.
#[derive(Debug, thiserror::Error)]
#[error("counter has not been incremented yet")]
struct NotStarted;

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

impl Query<GetLabel> for Counter {
    type Response = String;
    type Error = NotStarted;

    fn query(&self, _msg: GetLabel) -> Result<String, NotStarted> {
        if self.value == 0 {
            return Err(NotStarted);
        }
        Ok(format!("counter-{}", self.value))
    }
}

// Fixture: TallyCounter — records how many times its query was answered.
//
// The process builds its state with `A::default()`, so the count cannot live on
// the instance and be read back from the test. It lives here instead, and only
// `exec_delivers_the_query_response_exactly_once` touches this aggregate.

static TALLY_ANSWERED: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct TallyCounter {
    value: u64,
}

impl Aggregate for TallyCounter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

/// Query: the current count, recording each time it is answered.
struct Tally;

#[async_trait]
impl Decider<Increment> for TallyCounter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

impl Query<Tally> for TallyCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: Tally) -> Result<u64, Self::Error> {
        TALLY_ANSWERED.fetch_add(1, Ordering::SeqCst);
        Ok(self.value)
    }
}

// The acceptance scenario: the Response comes back, exactly once (L-5)

/// Given a `TallyCounter` advanced to 3, When one `exec(Tally)` is issued,
/// Then the answer is the state's own value and `Query::query` ran once —
/// not zero times (a cached or defaulted answer) and not twice (a retry).
#[tokio::test]
async fn exec_delivers_the_query_response_exactly_once() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = AggregateProps::<TallyCounter>::new(AggregateId::new("query-tally"), store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;

    proxy.ask(Increment).await.expect("ask 1 must succeed");
    proxy.ask(Increment).await.expect("ask 2 must succeed");
    proxy.ask(Increment).await.expect("ask 3 must succeed");

    let before = TALLY_ANSWERED.load(Ordering::SeqCst);

    // When
    let answer = proxy.exec(Tally).await.expect("exec(Tally) must succeed");

    // Then
    assert_eq!(
        answer, 3,
        "exec must return Query::Response computed from the process's current state"
    );
    assert_eq!(
        TALLY_ANSWERED.load(Ordering::SeqCst) - before,
        1,
        "one exec must ask the state exactly once (L-5)"
    );

    // And a second exec asks again rather than replaying the first answer.
    let answer = proxy
        .exec(Tally)
        .await
        .expect("second exec(Tally) must succeed");

    assert_eq!(answer, 3, "state must be unchanged by a query");
    assert_eq!(
        TALLY_ANSWERED.load(Ordering::SeqCst) - before,
        2,
        "each exec must ask the state exactly once (L-5)"
    );
}

// Query::Error is the domain's answer, and it arrives as ExecError::Domain

/// Given a `Counter` that has not been incremented, When `exec(GetLabel)` asks a
/// question it cannot answer, Then the domain's own error arrives as
/// `ExecError::Domain` — not flattened into `ExecError::Send`, which means the
/// machinery failed.
#[tokio::test]
async fn exec_delivers_query_error_as_exec_error_domain() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = AggregateProps::<Counter>::new(AggregateId::new("query-unlabelled"), store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;

    // When
    let outcome = proxy.exec(GetLabel).await;

    // Then
    match outcome {
        Err(ExecError::Domain(NotStarted)) => {}
        Err(ExecError::Send(err)) => panic!(
            "a domain error must not be reported as a transport failure: {:?}",
            err
        ),
        Ok(label) => panic!("exec(GetLabel) must fail on a counter at 0, got {label:?}"),
    }
}

// The question is synchronous and pure (L-1)

/// Given a `Counter` advanced by applying events directly — no process, no async
/// runtime, no `Context` — When the same question is asked twice, Then both
/// answers are the state's value and the state has not moved.
///
/// This is a plain `#[test]`: it does not compile if answering a question needs
/// an await point, and it does not build a `Context` because `Query::query` has
/// nowhere to receive one.
#[test]
fn query_answers_synchronously_without_a_runtime() {
    // Given
    let mut counter = Counter::default();
    counter.apply(Incremented);
    counter.apply(Incremented);
    counter.apply(Incremented);

    // When
    let first = counter.query(GetCount);
    let second = counter.query(GetCount);

    // Then
    assert_eq!(first, Ok(3), "query must report the current state");
    assert_eq!(
        second, first,
        "the same state and the same message must yield the same answer (L-1)"
    );
    assert_eq!(
        counter.value, 3,
        "query(&self) must not move the state it reports"
    );
}
