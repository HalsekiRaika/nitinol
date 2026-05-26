// E2E test: Aggregate-to-Aggregate communication via Effect::Side.
//
// Demonstrates three key behaviours:
//   1. decide() returns Effect::Side(custom impl) → fires after ask() returns,
//      calling AggregateProxy::tell on a second Aggregate
//   2. Effect::publish reaches a stream subscriber
//   3. Side effect errors are NOT propagated to the ask() caller (fire-and-forget)
//
// Implementation note: Effect::tell(proxy.0, AskCmd(...)) is crate-private because
// AggregateProcess is not exported. The equivalent user-visible pattern is a custom
// SideEffect that holds an AggregateProxy and calls proxy.tell(cmd) in execute().

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect,
    Event, Receive as EvtReceive, SideEffect, SideEffectError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Stream};
use nitinol_runtime::{BoxedMessage, Props, ProcessSystem};

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("E2EComm.Incremented");
}

// ---------------------------------------------------------------------------
// Fixtures: Counter aggregate (shared for both "A" and "B" roles in tests)
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
// Fixtures: commands
// ---------------------------------------------------------------------------

struct Increment;
struct GetCount;

/// Command that triggers a Side effect sending Increment to a target aggregate.
struct DelegateToB {
    target: AggregateProxy<Counter>,
    done_notify: Arc<Notify>,
}

/// Command that triggers a Side effect that always fails.
struct TriggerFailingSide;

/// Command that triggers Effect::publish to broadcast a Notification.
struct PublishNotification {
    stream: ProcessProxy<Stream<BoxedMessage>>,
}

// ---------------------------------------------------------------------------
// Fixtures: Decider implementations on Counter
// ---------------------------------------------------------------------------

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
impl EvtReceive<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

/// decide(DelegateToB): return a custom Side effect that calls target.tell(Increment).
#[async_trait]
impl Decider<DelegateToB> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: DelegateToB,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::Side(Box::new(TellTargetEffect {
            target: cmd.target,
            done_notify: cmd.done_notify,
        })))
    }
}

/// decide(TriggerFailingSide): return a Side effect that always fails.
#[async_trait]
impl Decider<TriggerFailingSide> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: TriggerFailingSide,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::Side(Box::new(AlwaysFailSideEffect)))
    }
}

/// decide(PublishNotification): return Effect::publish(stream, Notification).
#[async_trait]
impl Decider<PublishNotification> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PublishNotification,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::publish(cmd.stream, nitinol_runtime::BoxedMessage::new(Notification)))
    }
}

// ---------------------------------------------------------------------------
// Fixtures: SideEffect implementations
// ---------------------------------------------------------------------------

/// Calls `target.tell(Increment)` then notifies `done_notify` that the effect fired.
struct TellTargetEffect {
    target: AggregateProxy<Counter>,
    done_notify: Arc<Notify>,
}

impl SideEffect for TellTargetEffect {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.target
                .tell(Increment)
                .await
                .map_err(|e| SideEffectError::Send(Box::new(e)))?;
            self.done_notify.notify_one();
            Ok(())
        })
    }
}

/// Always returns an error — used to verify fire-and-forget semantics.
struct AlwaysFailSideEffect;

impl SideEffect for AlwaysFailSideEffect {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async { Err(SideEffectError::NotFound) })
    }
}

/// A notification message published to a stream.
#[derive(Clone)]
struct Notification;

// ---------------------------------------------------------------------------
// Fixtures: JsonCodec
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Fixtures: NotificationSubscriber — receives BoxedMessage from a stream
// ---------------------------------------------------------------------------

struct NotificationSubscriber {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Process for NotificationSubscriber {}

impl Receive<BoxedMessage> for NotificationSubscriber {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> Result<(), Self::Error> {
        if msg.downcast_ref::<Notification>().is_some() {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_one();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: spawn a Counter aggregate on a fresh InMemoryEventStore
// ---------------------------------------------------------------------------

async fn spawn_counter(system: &EventSourceSystem<JsonCodec>, id: &str) -> AggregateProxy<Counter> {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    system
        .spawn_aggregate::<Counter>(AggregateId::new(id), store)
        .await
}

// ---------------------------------------------------------------------------
// Test 1: tell side effect from decide() reaches the target aggregate
// ---------------------------------------------------------------------------

/// Given Aggregate A's decide(DelegateToB) returns Effect::Side(TellTargetEffect),
/// When proxy_a.ask(DelegateToB { target: proxy_b, ... }) is called,
/// Then:
///   - ask() returns Ok with an empty event list (no persisted events)
///   - the side effect fires and proxy_b receives the Increment command
///   - proxy_b.exec(GetCount) returns 1
#[tokio::test]
async fn e2e_tell_side_effect_from_decide_reaches_target_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let proxy_a = spawn_counter(&system, "e2e-comm-tell-a").await;
    let proxy_b = spawn_counter(&system, "e2e-comm-tell-b").await;
    let done = Arc::new(Notify::new());

    // When: ask A to delegate via side effect
    let events = proxy_a
        .ask(DelegateToB {
            target: proxy_b.clone(),
            done_notify: Arc::clone(&done),
        })
        .await
        .expect("ask must succeed");

    // Then: ask returns empty — DelegateToB produces no persisted events
    assert!(
        events.is_empty(),
        "DelegateToB must not produce persisted events; got {:?}",
        events
    );

    // Wait for the side effect to fire (with timeout)
    tokio::time::timeout(Duration::from_millis(300), done.notified())
        .await
        .expect("side effect must fire within 300 ms");

    // Verify B received the Increment via tell
    let count_b = proxy_b.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        count_b, 1,
        "B must have received and applied one Increment via the tell side effect"
    );
}

// ---------------------------------------------------------------------------
// Test 2: side effect failure is NOT propagated to the ask() caller
// ---------------------------------------------------------------------------

/// Given Aggregate A's decide(TriggerFailingSide) returns a Side effect that always fails,
/// When proxy_a.ask(TriggerFailingSide) is called,
/// Then ask() returns Ok — side effect errors are fire-and-forget,
/// not visible to the caller.
#[tokio::test]
async fn e2e_side_effect_failure_not_propagated_to_ask_caller() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let proxy_a = spawn_counter(&system, "e2e-comm-fail").await;

    // When
    let result = proxy_a.ask(TriggerFailingSide).await;

    // Then: ask() must succeed even though the side effect fails
    assert!(
        result.is_ok(),
        "ask() must succeed when the Side effect fails (fire-and-forget); got {:?}",
        result.err()
    );
    let events = result.unwrap();
    assert!(
        events.is_empty(),
        "TriggerFailingSide must return no persisted events"
    );
}

// ---------------------------------------------------------------------------
// Test 3: publish side effect from decide() reaches a stream subscriber
// ---------------------------------------------------------------------------

/// Given a NotificationSubscriber process subscribed to a BoxedMessage stream,
/// and an Aggregate A whose decide(PublishNotification) returns Effect::publish,
/// When proxy_a.ask(PublishNotification { stream }) is called,
/// Then the subscriber receives the Notification via BoxedMessage.
#[tokio::test]
async fn e2e_publish_side_effect_from_decide_reaches_stream_subscriber() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let proxy_a = spawn_counter(&system, "e2e-comm-pub").await;

    // Spawn the BoxedMessage stream
    let stream = system
        .process_system()
        .spawn_stream::<BoxedMessage>(ProcessName::new("e2e-comm-pub-stream"))
        .await
        .expect("spawn_stream must succeed");

    // Spawn the subscriber and subscribe it to the stream
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sub_proxy = system.process_system().spawn(Props::new({
        let count = Arc::clone(&count);
        let notify = Arc::clone(&notify);
        move || NotificationSubscriber {
            count: Arc::clone(&count),
            notify: Arc::clone(&notify),
        }
    })).await;

    stream
        .subscribe(sub_proxy)
        .await
        .expect("subscribe must succeed");

    // When: ask A to publish a Notification to the stream
    proxy_a
        .ask(PublishNotification { stream })
        .await
        .expect("ask must succeed");

    // Then: the subscriber receives the Notification
    tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= 1 {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("subscriber must receive Notification within 300 ms");

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "subscriber must receive exactly one Notification"
    );
}
