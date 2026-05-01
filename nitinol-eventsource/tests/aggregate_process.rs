// Integration tests for AggregateProcess, AggregateProps, and AggregateProxy.
//
// Tests verify the full request lifecycle: spawn → ask/tell/exec → persistence → state.
//
// These tests reference types from nitinol_eventsource::process that are implemented
// in Phase 2.4. Compile errors are expected until the implementation is in place.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;

use nitinol_eventsource::{
    Aggregate, Context, Decider, Effect, Event,
    Receive as EvtReceive,
    SideEffect, SideEffectError,
    AggregateProps, AggregateProxy, AskError, TellError,
    EventCodec, EncodeError, DecodeError,
};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::{AggregateId, EventType};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

/// A unit event representing one successful increment.
#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("Incremented");
}

// ---------------------------------------------------------------------------
// Fixtures: aggregate
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Fixtures: commands and messages
// ---------------------------------------------------------------------------

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

#[async_trait]
impl Decider<IncrementBy> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: IncrementBy,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist_all(vec![Incremented; cmd.0 as usize]))
    }
}

#[async_trait]
impl Decider<IncrementIfLessThan> for Counter {
    type Rejection = AtMaxError;

    async fn decide(
        &self,
        cmd: IncrementIfLessThan,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        if self.value >= cmd.0 {
            Err(AtMaxError(self.value))
        } else {
            Ok(Effect::persist(Incremented))
        }
    }
}

#[async_trait]
impl EvtReceive<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: test codec
// ---------------------------------------------------------------------------

/// Test codec for Incremented events.
/// Encodes as empty bytes (unit struct has no data); decodes by ignoring the payload.
struct TestCodec;

impl EventCodec<Incremented> for TestCodec {
    fn encode(&self, _event: &Incremented) -> Result<Bytes, EncodeError> {
        Ok(Bytes::new())
    }

    fn decode(&self, _event_type: EventType, _bytes: Bytes) -> Result<Incremented, DecodeError> {
        Ok(Incremented)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: side-effect test helpers
// ---------------------------------------------------------------------------

/// A side effect that notifies a Notify handle when executed.
struct NotifySideEffect(Arc<tokio::sync::Notify>);

impl SideEffect for NotifySideEffect {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.0.notify_one();
            Ok(())
        })
    }
}

/// Command: persists one Incremented event and attaches a fire-and-forget side effect
/// that notifies the embedded Notify once it executes.
struct IncrementWithSide {
    notify: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Decider<IncrementWithSide> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: IncrementWithSide,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        // Persist first, then attach side effect — both in one Sequence.
        Ok(Effect::persist(Incremented)
            .combine(Effect::Side(Box::new(NotifySideEffect(cmd.notify)))))
    }
}

// ---------------------------------------------------------------------------
// Helper: spawn a Counter process
// ---------------------------------------------------------------------------

async fn spawn_counter(
    aggregate_id: AggregateId,
    event_store: Arc<dyn EventStore>,
) -> (ProcessSystem, AggregateProxy<Counter>) {
    let system = ProcessSystem::new().await;
    let proxy = AggregateProps::<Counter>::new(aggregate_id, event_store)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system)
        .await;
    (system, proxy)
}

// ---------------------------------------------------------------------------
// ask<Increment>: basic success path
// ---------------------------------------------------------------------------

/// ask(Increment) returns a Vec containing one Incremented event.
#[tokio::test]
async fn ask_increment_returns_vec_incremented() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-basic"), store).await;

    // When
    let events = proxy.ask(Increment).await.expect("ask(Increment) must succeed");

    // Then
    assert_eq!(
        events,
        vec![Incremented],
        "ask(Increment) must return [Incremented]"
    );
}

// ---------------------------------------------------------------------------
// ask<Increment>: events are persisted to the EventStore
// ---------------------------------------------------------------------------

/// After ask(Increment), the EventStore contains the persisted event.
/// Verified by spawning a second process on the same store and checking state via exec.
#[tokio::test]
async fn ask_persists_events_to_event_store() {
    // Given
    let inner = Arc::new(InMemoryEventStore::default());
    let store: Arc<dyn EventStore> = Arc::clone(&inner) as Arc<dyn EventStore>;
    let id = AggregateId::new("agg-ask-persist");
    let (_system, proxy) = spawn_counter(id.clone(), store).await;

    // When
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then: a fresh process for the same aggregate_id replays the stored event.
    let system2 = ProcessSystem::new().await;
    let proxy2 = AggregateProps::<Counter>::new(id, Arc::clone(&inner) as Arc<dyn EventStore>)
        .with_codec(Arc::new(TestCodec))
        .spawn(&system2)
        .await;

    let count = proxy2.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count, 1,
        "replayed process must see 1 event written by the first process"
    );
}

// ---------------------------------------------------------------------------
// ask<Increment>: sequential asks accumulate state
// ---------------------------------------------------------------------------

/// Three sequential ask(Increment) calls advance the counter to 3.
#[tokio::test]
async fn ask_sequential_increments_accumulate_state() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-seq"), store).await;

    // When
    proxy.ask(Increment).await.expect("ask 1 must succeed");
    proxy.ask(Increment).await.expect("ask 2 must succeed");
    proxy.ask(Increment).await.expect("ask 3 must succeed");

    // Then
    let count = proxy.exec(GetCount).await.expect("exec(GetCount) must succeed");
    assert_eq!(count, 3, "state must be 3 after three Increment asks");
}

// ---------------------------------------------------------------------------
// tell<Increment>: fire-and-forget returns Ok(())
// ---------------------------------------------------------------------------

/// tell(Increment) returns Ok(()) without a response payload.
#[tokio::test]
async fn tell_increment_returns_unit() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-tell-unit"), store).await;

    // When / Then
    let result: Result<(), TellError> = proxy.tell(Increment).await;
    result.expect("tell(Increment) must return Ok(())");
}

// ---------------------------------------------------------------------------
// tell then exec: state is reflected after tell
// ---------------------------------------------------------------------------

/// After tell(Increment), exec(GetCount) returns 1.
/// The mpsc task queue guarantees tell is processed before exec.
#[tokio::test]
async fn tell_then_exec_sees_updated_state() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-tell-exec"), store).await;

    // When: tell is queued first; exec is queued second and processed after tell.
    proxy.tell(Increment).await.expect("tell must succeed");
    let count = proxy.exec(GetCount).await.expect("exec must succeed");

    // Then
    assert_eq!(count, 1, "exec(GetCount) must return 1 after tell(Increment)");
}

// ---------------------------------------------------------------------------
// exec<GetCount>: does not mutate state
// ---------------------------------------------------------------------------

/// Two consecutive exec(GetCount) calls return the same value.
#[tokio::test]
async fn exec_does_not_mutate_state() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-exec-immut"), store).await;
    proxy.ask(Increment).await.expect("ask must succeed");
    proxy.ask(Increment).await.expect("ask must succeed");

    // When
    let first: u64 = proxy.exec(GetCount).await.expect("first exec must succeed");
    let second: u64 = proxy.exec(GetCount).await.expect("second exec must succeed");

    // Then
    assert_eq!(first, 2, "first exec must return 2");
    assert_eq!(second, 2, "second exec must return same value — exec must not mutate state");
}

// ---------------------------------------------------------------------------
// Multiple Decider implementations: ask with IncrementBy(n)
// ---------------------------------------------------------------------------

/// A second Decider (IncrementBy) coexists with Increment on the same Aggregate.
/// ask(IncrementBy(3)) emits 3 Incremented events in one Persist batch.
#[tokio::test]
async fn ask_second_decider_increment_by() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-incby"), store).await;

    // When
    let events = proxy
        .ask(IncrementBy(3))
        .await
        .expect("ask(IncrementBy(3)) must succeed");

    // Then: three events returned and state advanced by 3
    assert_eq!(
        events,
        vec![Incremented, Incremented, Incremented],
        "ask(IncrementBy(3)) must return 3 Incremented events"
    );
    let count = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(count, 3, "state must advance by 3 after IncrementBy(3)");
}

// ---------------------------------------------------------------------------
// ask: rejection returns AskError::Rejection
// ---------------------------------------------------------------------------

/// ask(IncrementIfLessThan(0)) is rejected because value (0) >= threshold (0).
#[tokio::test]
async fn ask_rejected_command_returns_rejection_error() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-ask-reject"), store).await;

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

// ---------------------------------------------------------------------------
// Effect::Side: fire-and-forget, does not block ask response
// ---------------------------------------------------------------------------

/// ask(IncrementWithSide) returns Vec<Incremented> without waiting for the side effect.
/// The side effect fires asynchronously and eventually notifies via Arc<Notify>.
/// Response contains only the Persist events; Side is excluded.
#[tokio::test]
async fn side_effect_is_fire_and_forget_and_does_not_appear_in_response() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-side-ff"), store).await;
    let notify = Arc::new(tokio::sync::Notify::new());

    // When
    let events = proxy
        .ask(IncrementWithSide { notify: notify.clone() })
        .await
        .expect("ask with side effect must succeed");

    // Then: response contains only the Persist event, not the Side result
    assert_eq!(
        events,
        vec![Incremented],
        "ask response must contain only Persist events; Side is excluded"
    );

    // Side effect fires asynchronously in a spawned task — wait for notification.
    tokio::time::timeout(Duration::from_millis(500), notify.notified())
        .await
        .expect("side effect must fire within 500 ms");
}

// ---------------------------------------------------------------------------
// ask/exec ordering: interleaved operations maintain consistent sequence
// ---------------------------------------------------------------------------

/// Interleaved ask + exec calls maintain correct sequence numbers and state.
/// This also validates that sequence is updated correctly across multiple Persist operations.
#[tokio::test]
async fn interleaved_ask_and_exec_maintain_consistent_state() {
    // Given
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let (_system, proxy) = spawn_counter(AggregateId::new("agg-interleaved"), store).await;

    // When
    proxy.ask(Increment).await.expect("ask 1 must succeed");
    let after_first = proxy.exec(GetCount).await.expect("exec after first ask");
    proxy.ask(IncrementBy(2)).await.expect("ask IncrementBy(2)");
    let after_second = proxy.exec(GetCount).await.expect("exec after IncrementBy");

    // Then
    assert_eq!(after_first, 1, "count must be 1 after first ask");
    assert_eq!(after_second, 3, "count must be 3 after IncrementBy(2)");
}
