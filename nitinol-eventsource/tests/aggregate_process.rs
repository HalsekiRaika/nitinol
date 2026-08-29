// Integration tests for AggregateProcess, AggregateProps, and AggregateProxy.
//
// Tests verify the full request lifecycle: spawn → ask/tell/exec → persistence → state.
//
// AggregateProps holds Arc<dyn EventStore> directly: the
// EventPersistor actor was removed and the process now calls store.append /
// store.load inline.

use std::sync::Arc;

use bytes::Bytes;

use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Query, TellError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName, Variant};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

/// A unit event representing one successful increment.
#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));
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

// Fixtures: commands and messages

/// Command: always succeeds, emits one Incremented event.
struct Increment;

/// Command: emits N Incremented events in a single Persist batch.
struct IncrementBy(u64);

/// Command: fails when value >= threshold (tests rejection path).
struct IncrementIfLessThan(u64);

/// Query message: returns current count without mutating state.
struct GetCount;

#[derive(Debug, thiserror::Error)]
#[error("counter already at max: {0}")]
struct AtMaxError(u64);

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Decider<IncrementBy> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: IncrementBy) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented; cmd.0 as usize]).output(())
    }
}

impl Decider<IncrementIfLessThan> for Counter {
    type Output = ();
    type Rejection = AtMaxError;

    fn decide(&self, cmd: IncrementIfLessThan) -> Decision<Incremented, (), AtMaxError> {
        if self.value >= cmd.0 {
            return Decision::reject(AtMaxError(self.value));
        }
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

// Fixtures: test codec

/// Pass-through codec for Incremented (unit struct — no data to encode).
/// Encodes to empty bytes; decodes by returning Incremented regardless of payload.
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

// Helper: spawn a Counter process holding a fresh Arc<dyn EventStore>

/// Creates a ProcessSystem and spawns a Counter AggregateProcess holding an
/// `Arc<dyn EventStore>` directly.
///
/// Returns (system, proxy).  Caller must keep `_system` alive.
async fn spawn_counter(aggregate_id: AggregateId) -> (ProcessSystem, AggregateProxy<Counter>) {
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = AggregateProps::<Counter>::new(aggregate_id, store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;
    (system, proxy)
}

// ask<Increment>: events are persisted and replayable via shared Arc<dyn EventStore>

/// After ask(Increment) on process 1, a second process sharing the same
/// `Arc<dyn EventStore>` replays the stored event and restores the count to 1.
#[tokio::test]
async fn ask_persists_events_to_event_store() {
    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("agg-ask-persist");

    let proxy = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;

    // When
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then: a fresh process for the same aggregate_id replays through the same Arc<dyn EventStore>.
    let proxy2 = AggregateProps::<Counter>::new(id, store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;

    let count = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 1,
        "replayed process must see 1 event written by the first process"
    );
}

// ask<Increment>: sequential asks accumulate state

/// Three sequential ask(Increment) calls advance the counter to 3.
#[tokio::test]
async fn ask_sequential_increments_accumulate_state() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-seq")).await;

    // When
    proxy.ask(Increment).await.expect("ask 1 must succeed");
    proxy.ask(Increment).await.expect("ask 2 must succeed");
    proxy.ask(Increment).await.expect("ask 3 must succeed");

    // Then
    let count = proxy
        .exec(GetCount)
        .await
        .expect("exec(GetCount) must succeed");
    assert_eq!(count, 3, "state must be 3 after three Increment asks");
}

// tell<Increment>: fire-and-forget returns Ok(())

/// tell(Increment) returns Ok(()) without a response payload.
#[tokio::test]
async fn tell_increment_returns_unit() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-tell-unit")).await;

    // When / Then
    let result: Result<(), TellError> = proxy.tell(Increment).await;
    result.expect("tell(Increment) must return Ok(())");
}

// tell then exec: state is reflected after tell

/// After tell(Increment), exec(GetCount) returns 1.
/// The mpsc task queue guarantees tell is processed before exec.
#[tokio::test]
async fn tell_then_exec_sees_updated_state() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-tell-exec")).await;

    // When: tell is queued first; exec is queued second and processed after tell.
    proxy.tell(Increment).await.expect("tell must succeed");
    let count = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then
    assert_eq!(
        count, 1,
        "exec(GetCount) must return 1 after tell(Increment)"
    );
}

// exec<GetCount>: does not mutate state

/// Two consecutive exec(GetCount) calls return the same value.
#[tokio::test]
async fn exec_does_not_mutate_state() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-exec-immut")).await;
    proxy.ask(Increment).await.expect("ask must succeed");
    proxy.ask(Increment).await.expect("ask must succeed");

    // When
    let first: u64 = proxy.exec(GetCount).await.expect("first exec must succeed");
    let second: u64 = proxy
        .exec(GetCount)
        .await
        .expect("second exec must succeed");

    // Then
    assert_eq!(first, 2, "first exec must return 2");
    assert_eq!(
        second, 2,
        "second exec must return same value — exec must not mutate state"
    );
}

// Multiple Decider implementations: ask with IncrementBy(n)

/// A second Decider (IncrementBy) coexists with Increment on the same Aggregate.
/// ask(IncrementBy(3)) advances the state by 3.
#[tokio::test]
async fn ask_second_decider_increment_by() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-incby")).await;

    // When
    proxy
        .ask(IncrementBy(3))
        .await
        .expect("ask(IncrementBy(3)) must succeed");

    // Then
    let count = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "state must advance by 3 after IncrementBy(3)");
}

// ask: rejection returns AskError::Rejection

/// ask(IncrementIfLessThan(0)) is rejected because value (0) >= threshold (0).
#[tokio::test]
async fn ask_rejected_command_returns_rejection_error() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-reject")).await;

    // When
    let result = proxy.ask(IncrementIfLessThan(0)).await;

    // Then
    match result {
        Err(AskError::Rejection(err)) => {
            assert_eq!(err.0, 0, "rejection must carry current value (0)");
        }
        Ok(_) => panic!("ask must fail when value >= threshold"),
        Err(other) => panic!("unexpected error variant: {:?}", other),
    }
}

// ask/exec ordering: interleaved operations maintain consistent sequence

/// Interleaved ask + exec calls maintain correct sequence numbers and state.
/// This also validates that sequence is updated correctly across multiple Persist operations.
#[tokio::test]
async fn interleaved_ask_and_exec_maintain_consistent_state() {
    // Given
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-interleaved")).await;

    // When
    proxy.ask(Increment).await.expect("ask 1 must succeed");
    let after_first = proxy.exec(GetCount).await.expect("exec after first ask");
    proxy.ask(IncrementBy(2)).await.expect("ask IncrementBy(2)");
    let after_second = proxy.exec(GetCount).await.expect("exec after IncrementBy");

    // Then
    assert_eq!(after_first, 1, "count must be 1 after first ask");
    assert_eq!(after_second, 3, "count must be 3 after IncrementBy(2)");
}

// Regression: aggregate persist path stores enum event's variant() in LoadedEvent

/// An enum event with arm-specific `variant()` override.
#[derive(Clone, PartialEq, Debug)]
enum CounterOp {
    Incremented,
}

impl Event for CounterOp {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("CounterOp"));

    fn variant(&self) -> EventType {
        let v = match self {
            CounterOp::Incremented => Variant::new("Incremented"),
        };
        EventType::with_variant(Family::new(""), TypeName::new("CounterOp"), v)
    }
}

#[derive(Default)]
struct CounterEnum {
    value: i64,
}

impl Aggregate for CounterEnum {
    type Event = CounterOp;

    fn apply(&mut self, event: CounterOp) {
        match event {
            CounterOp::Incremented => self.value += 1,
        }
    }
}

struct IncOp;

impl Decider<IncOp> for CounterEnum {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: IncOp) -> Decision<CounterOp, (), Self::Rejection> {
        Decision::persist(vec![CounterOp::Incremented]).output(())
    }
}

struct CounterOpCodec;

impl Codec<CounterOp> for CounterOpCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &CounterOp) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<CounterOp, Self::Error> {
        Ok(CounterOp::Incremented)
    }
}

/// Regression test: the aggregate persist path must store the arm-specific
/// `variant()` of an enum event in `LoadedEvent.event_type`, not the
/// type-level `EVENT_TYPE` (which always has `variant: None`).
///
/// Failure mode without the fix: `event_type: A::Event::EVENT_TYPE` ignores
/// the `variant()` override, so the stored event_type has `variant=None` even
/// for enum events.
#[tokio::test]
async fn aggregate_persist_stores_enum_event_variant_in_loaded_event() {
    use futures_util::TryStreamExt;
    use nitinol_persistence::LoadQuery;

    // Given
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("agg-enum-variant-persist");

    let proxy = AggregateProps::<CounterEnum>::new(id.clone(), Arc::clone(&store))
        .with_codec(Arc::new(CounterOpCodec))
        .spawn(&system)
        .await;

    // When: persist an enum event via AggregateProcess
    proxy.ask(IncOp).await.expect("ask(IncOp) must succeed");

    // Then: the stored LoadedEvent carries the arm-specific variant=Some("Incremented")
    let events: Vec<_> = store
        .load(LoadQuery::by_stream(id.as_str()))
        .await
        .expect("store.load must succeed")
        .try_collect()
        .await
        .expect("stream collect must succeed");

    assert_eq!(events.len(), 1, "exactly one event must be stored");
    assert_eq!(
        events[0].event_type.variant(),
        Some(Variant::new("Incremented")),
        "aggregate persist must store enum event's arm variant (Some), not None; \
         regression for aggregate_process.rs using event.variant() instead of EVENT_TYPE"
    );
    assert_eq!(
        events[0].event_type.type_key(),
        CounterOp::EVENT_TYPE.type_key(),
        "type_key must match EVENT_TYPE so routing dispatch still works"
    );
}
