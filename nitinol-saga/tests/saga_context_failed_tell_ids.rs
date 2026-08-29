//! `SagaContext::failed_tell_ids`: the saga must be able to detect
//! `TellFailed` outcomes in a subsequent `Saga::handle` call.
//!
//! Two paths are tested:
//!
//! 1. **Replay path** — a `TellFailed` marker is already in the saga event
//!    stream on restart.  The first `handle` call after `on_start` must expose
//!    the corresponding `tell_id` in `ctx.failed_tell_ids()`.
//!
//! 2. **Runtime path** — the outbox executor exhausts all retry attempts and
//!    appends a `TellFailed` marker during the current run.  That failure is
//!    delivered to `Saga::on_tell_failed` the moment it settles, so it is
//!    already accounted for: the next `handle` call must **not** see the same
//!    `tell_id` again in `ctx.failed_tell_ids()`.  Only replay-sourced
//!    failures reach the saga through this slice.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_outbox_tell_failed, encode_outbox_tell_requested, outbox_kind_of, JsonCodec, OutboxKind,
    OUTBOX_MARKER,
};

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Decider, Decision, Event,
    SequenceCursor, TellError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::error::SendError;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("failed_tell_ids"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderProcessed {
    order_id: String,
}

impl Event for OrderProcessed {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("failed_tell_ids"),
        TypeName::new("OrderProcessed"),
    );
}

// Minimal aggregate — used only so we can build a real TellIntent.

#[derive(Default)]
struct DummyTarget;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DummyCmd;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DummyEvent;

impl Event for DummyEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("failed_tell_ids"), TypeName::new("DummyEvent"));
}

impl Aggregate for DummyTarget {
    type Event = DummyEvent;
    fn apply(&mut self, _event: DummyEvent) {}
}

impl Decider<DummyCmd> for DummyTarget {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: DummyCmd) -> Decision<DummyEvent, (), Self::Rejection> {
        Decision::persist(Vec::new()).output(())
    }
}

// A TellTarget that always returns an error — forces TellFailed via executor.

struct FailingTarget<A: Aggregate> {
    target_id: AggregateId,
    _phantom: PhantomData<fn() -> A>,
}

impl<A: Aggregate> Clone for FailingTarget<A> {
    fn clone(&self) -> Self {
        Self {
            target_id: self.target_id.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> FailingTarget<A> {
    fn new() -> Self {
        Self {
            target_id: AggregateId::new("test-failing-target"),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for FailingTarget<A> {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move { Err(TellError::Send(SendError)) })
    }

    fn aggregate_id(&self) -> &AggregateId {
        &self.target_id
    }
}

// Shared capture state: collects the `failed_tell_ids` seen by each `handle`.

type Captured = Arc<Mutex<Vec<Vec<u64>>>>;

// Replay-path test saga: records `ctx.failed_tell_ids()` on each handle call.

struct RecordingSaga {
    captured: Captured,
}

/// Correlation rule of [`RecordingSaga`]: the one order process every
/// `OrderPlaced` published by the replay-path tests belongs to.
const RECORDING_SAGA_ID: &str = "failed-tell-replay-saga-1";

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = OrderProcessed;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RECORDING_SAGA_ID))
    }

    fn apply(&mut self, _event: OrderProcessed) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<OrderProcessed>, Self::Error> {
        self.captured
            .lock()
            .await
            .push(ctx.failed_tell_ids().to_vec());
        Ok(SagaEffect::None)
    }
}

// Runtime-path test saga: emits a failing tell on the first event, then
// records failed_tell_ids on the second event.

struct RuntimeSaga {
    /// Shared with the test.  Each `handle` call appends the current
    /// `failed_tell_ids()` slice so the test can assert on it.
    captured: Captured,
    /// True after the first handle has already emitted the failing tell.
    told: bool,
}

impl RuntimeSaga {
    fn new(captured: Captured) -> Self {
        Self {
            captured,
            told: false,
        }
    }
}

/// Correlation rule of [`RuntimeSaga`]: the one order process every
/// `OrderPlaced` published by the runtime-path test belongs to.
const RUNTIME_SAGA_ID: &str = "failed-tell-runtime-saga-1";

#[async_trait]
impl Saga for RuntimeSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = OrderProcessed;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RUNTIME_SAGA_ID))
    }

    fn apply(&mut self, _event: OrderProcessed) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<OrderProcessed>, Self::Error> {
        self.captured
            .lock()
            .await
            .push(ctx.failed_tell_ids().to_vec());

        if !self.told {
            self.told = true;
            // Emit a tell that will always fail — the executor will exhaust
            // its retries and append TellFailed.
            let intent =
                TellIntent::new::<DummyTarget, DummyCmd, _>(FailingTarget::new(), DummyCmd);
            Ok(SagaEffect::persist(OrderProcessed {
                order_id: "runtime-1".to_owned(),
            })
            .combine(SagaEffect::tell_intent(intent)))
        } else {
            Ok(SagaEffect::None)
        }
    }
}

// Helpers

async fn append_raw(
    store: &Arc<dyn EventStore>,
    stream_key: &str,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

/// Append an upstream `OrderPlaced` event to trigger `handle`.
async fn publish_order_placed(upstream_store: &Arc<dyn EventStore>, order_id: &str, sequence: u64) {
    let payload = serde_json::to_vec(&OrderPlaced {
        order_id: order_id.to_owned(),
    })
    .expect("serialization must succeed");
    append_raw(
        upstream_store,
        "upstream",
        sequence,
        EventType::new(Family::new("failed_tell_ids"), TypeName::new("OrderPlaced")),
        Bytes::from(payload),
    )
    .await;
}

// Test 1: replay path — TellFailed in event history is surfaced via context.

/// When the saga restarts and its event history contains a `TellFailed` marker,
/// the first `Saga::handle` invocation after `on_start` must expose the
/// corresponding `tell_id` in `ctx.failed_tell_ids()`.
///
/// This confirms the saga can detect `TellFailed` in the next `handle` call
/// and decide on compensation.
#[tokio::test]
async fn replay_tell_failed_is_surfaced_in_next_handle_via_context() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    // Pre-seed: TellRequested(tell_id=1) + TellFailed(tell_id=1).
    // This simulates a saga that had a tell fail in a previous run.
    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_MARKER,
        encode_outbox_tell_requested(1, None),
    )
    .await;
    append_raw(
        &saga_store,
        saga_id.as_str(),
        2,
        OUTBOX_MARKER,
        encode_outbox_tell_failed(1),
    )
    .await;

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_saga = Arc::clone(&captured);

    let _saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_saga),
            }
        })
        .with_codec(system.codec::<OrderProcessed>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: "upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // Allow on_start (replay) to complete before publishing the upstream event.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Trigger handle.
    publish_order_placed(&upstream_store, "order-replay-1", 1).await;

    // Wait for handle to be invoked.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let guard = captured.lock().await;
        if !guard.is_empty() {
            break;
        }
        drop(guard);
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for saga handle invocation");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let snapshots = captured.lock().await.clone();
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one handle call must have been recorded"
    );
    assert_eq!(
        snapshots[0],
        vec![1u64],
        "handle must see tell_id=1 in ctx.failed_tell_ids() because the event \
         history contains TellFailed(tell_id=1) from before the restart"
    );
}

// Test 2: replay synthetic TellFailed surfaces via context.

/// Regression test for call-chain integrity on the replay path.
///
/// When the replay path appends a **synthetic** `TellFailed` for an
/// unresolvable `TellRequested` (no `PendingIntents`, no crash-restart factory,
/// no crash-restart bytes), the `failed_tell_ids` accumulator must be updated
/// so that the next `Saga::handle` invocation exposes the `tell_id` via
/// `ctx.failed_tell_ids()`.
///
/// Previously `append_terminal`'s return value was discarded in the synthetic
/// path, so the accumulator was never updated and the Saga had no way to detect
/// the failure in a subsequent `handle` call.
#[tokio::test]
async fn synthetic_replay_tell_failed_is_surfaced_in_next_handle_via_context() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    // Seed only a TellRequested — no TellAcked/TellFailed yet, no crash-restart
    // bytes (prost TellRequested with field 2 absent).  Simulates a crash between
    // the atomic Persist batch and the executor's terminal append where the new
    // process has no in-memory intent and no factory registered.
    let tell_id_payload = encode_outbox_tell_requested(5, None);
    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_MARKER,
        tell_id_payload,
    )
    .await;

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_saga = Arc::clone(&captured);

    // Spawn without crash-restart factory and without pre-populated PendingIntents.
    // The replay path must append synthetic TellFailed(tell_id=5) AND push tell_id
    // to the shared accumulator.
    let _saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_saga),
            }
        })
        .with_codec(system.codec::<OrderProcessed>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: "upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // Allow on_start (replay + synthetic TellFailed append) to complete before
    // triggering the first handle call.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Trigger handle.
    publish_order_placed(&upstream_store, "order-synthetic-1", 1).await;

    // Wait for handle to be invoked.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let guard = captured.lock().await;
        if !guard.is_empty() {
            break;
        }
        drop(guard);
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for saga handle invocation");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let snapshots = captured.lock().await.clone();
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one handle call must have been recorded"
    );
    assert_eq!(
        snapshots[0],
        vec![5u64],
        "handle must see tell_id=5 in ctx.failed_tell_ids() because the replay \
         path appended synthetic TellFailed(tell_id=5) and must update the \
         failed-tell-ids accumulator"
    );
}

// Test 3: runtime path — an executor-appended TellFailed is reported through
// the tell-failure hook, so it is not repeated to the next handle.

/// When the outbox executor exhausts all retry attempts and appends a
/// `TellFailed` marker during the current run, the failure is handed to
/// `Saga::on_tell_failed` immediately.  The **next** `Saga::handle` invocation
/// must therefore see an empty `ctx.failed_tell_ids()` — the same failure is
/// never announced twice.
///
/// `RuntimeSaga` leaves `on_tell_failed` at its trait default, so this pins
/// the deduplication itself rather than any compensating behaviour: an
/// implementation that notifies the hook but leaves the entry in the drain
/// path would surface the `tell_id` here a second time.
#[tokio::test]
async fn runtime_tell_failed_is_not_repeated_to_the_next_handle_via_context() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new(RUNTIME_SAGA_ID);
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_saga = Arc::clone(&captured);

    let _saga_proxy =
        SagaProps::<RuntimeSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            RuntimeSaga::new(Arc::clone(&captured_for_saga))
        })
        .with_codec(system.codec::<OrderProcessed>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: "upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // First event: saga emits a tell that always fails.
    publish_order_placed(&upstream_store, "order-runtime-1", 1).await;

    // Wait for the first handle call to complete (captured.len() == 1).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !captured.lock().await.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for first handle invocation");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for the executor to exhaust retries and append TellFailed.
    // RetryPolicy default: 3 attempts, 100ms / 200ms backoff → ~400ms total.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = saga_store
            .load(LoadQuery::by_stream(&saga_id))
            .await
            .expect("load must succeed");
        use futures_util::TryStreamExt;
        let events: Vec<_> = events.try_collect().await.expect("collect must succeed");
        let has_failed = events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))));
        if has_failed {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for TellFailed to be appended (event types: {:?})",
                types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Small buffer so failed_tell_ids accumulator is populated.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second event: should see the failed tell_id in context.
    publish_order_placed(&upstream_store, "order-runtime-2", 2).await;

    // Wait for the second handle call.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if captured.lock().await.len() >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for second handle invocation");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let snapshots = captured.lock().await.clone();
    assert!(
        snapshots.len() >= 2,
        "at least two handle calls must have been recorded"
    );
    // First handle: no prior TellFailed, so failed_tell_ids must be empty.
    assert!(
        snapshots[0].is_empty(),
        "first handle must see an empty failed_tell_ids (no tell has failed yet)"
    );
    // Second handle: the executor appended TellFailed between the two events,
    // and the tell-failure hook already consumed it.
    assert!(
        snapshots[1].is_empty(),
        "second handle must see an empty failed_tell_ids: the failure was \
         already delivered to on_tell_failed when the executor settled, and \
         announcing it again here would be a duplicate notification; got {:?}",
        snapshots[1]
    );
}
