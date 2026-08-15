//! Staged retry: when every dispatch attempt fails, the executor
//! retries up to the default RetryPolicy limit and then appends a `TellFailed`
//! outbox marker to the saga stream.
//!
//! A succeeding tell that previously failed is NOT in scope of this test (we
//! exercise only the unrecoverable path); the success path is covered in
//! e2e_saga.rs / saga_outbox_persist_atomicity.rs.

#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Context, Decider, Effect, Event,
    SequenceCursor, TellError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::error::SendError;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// A custom TellTarget that always returns Err(TellError::Send(SendError)).
// Counts every dispatch attempt so the test can verify the retry executor
// actually retried — and exactly that many times.

struct FailingTellTarget<A: Aggregate> {
    attempts: Arc<AtomicUsize>,
    _phantom: PhantomData<fn() -> A>,
}

// Manual Clone — `derive(Clone)` would impose `A: Clone`, which `Aggregate` does not require.
// `PhantomData<fn() -> A>` is always `Clone` regardless of `A`, so the inner state is trivially
// cloneable.
impl<A: Aggregate> Clone for FailingTellTarget<A> {
    fn clone(&self) -> Self {
        Self {
            attempts: Arc::clone(&self.attempts),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> FailingTellTarget<A> {
    fn new() -> Self {
        Self {
            attempts: Arc::new(AtomicUsize::new(0)),
            _phantom: PhantomData,
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for FailingTellTarget<A> {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        let attempts = Arc::clone(&self.attempts);
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(TellError::Send(SendError))
        })
    }

    fn aggregate_id_str(&self) -> &str {
        "test-failing-target"
    }
}

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("retry"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("retry"), TypeName::new("ReservationRequested"));
}

#[derive(Default)]
struct Inventory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved;

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("retry"), TypeName::new("InventoryReserved"));
}

impl Aggregate for Inventory {
    type Event = InventoryReserved;
    fn apply(&mut self, _event: InventoryReserved) {}
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Reserve;

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        _cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<InventoryReserved>, Self::Rejection> {
        Ok(Effect::persist(InventoryReserved))
    }
}

/// Correlation rule of [`FailingSaga`]: the single retry process instance every
/// `OrderPlaced` in this test belongs to.
const FAILING_SAGA_ID: &str = "retry-saga-1";

struct FailingSaga {
    target: FailingTellTarget<Inventory>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for FailingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(FAILING_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let effect = SagaEffect::tell(self.target.clone(), Reserve);
        let full = SagaEffect::persist(ReservationRequested { sku: event.sku }).combine(effect);
        self.handled.notify_one();
        Ok(full)
    }
}

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
    sequence: u64,
    sku: &str,
) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderPlaced must succeed");
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

async fn wait_for_tell_failed(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        let failed = events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))));
        if failed {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for TellFailed outbox event in saga stream (event_types: {:?})",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn tell_failing_every_attempt_yields_tell_failed_outbox_event_and_no_ack() {
    // Pinned to the default RetryPolicy:
    //   max_attempts: 3, initial_backoff: 100ms, multiplier: 2.0
    // 1 initial + 2 retries = 3 total attempts; total backoff: 100ms + 200ms.
    // Use a generous timeout to absorb scheduler jitter.
    const EXPECTED_ATTEMPTS: usize = 3;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("retry-order");
    append_order_placed(&upstream_store, &order_id, 1, "RETRY-SKU").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(FAILING_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let target = FailingTellTarget::<Inventory>::new();
    let target_for_saga = target.clone();
    let handled_for_saga = Arc::clone(&handled);

    let _saga_proxy =
        SagaProps::<FailingSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            FailingSaga {
                target: target_for_saga.clone(),
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Wait for the TellFailed to be appended (this also implies all retries completed).
    let events = wait_for_tell_failed(&saga_store, &saga_id, Duration::from_secs(5)).await;

    let failed_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();
    let acked_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .count();
    let requested_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_))))
        .count();

    assert_eq!(
        requested_count, 1,
        "Persist with one tell must emit exactly one TellRequested marker"
    );
    assert_eq!(
        failed_count, 1,
        "after exceeding RetryPolicy::max_attempts, exactly one TellFailed must be appended"
    );
    assert_eq!(
        acked_count, 0,
        "no TellAcked must be emitted when every dispatch attempt fails"
    );

    // Allow a small additional grace window in case the executor's last attempt
    // hadn't bumped the counter yet when we observed TellFailed (unlikely, but
    // the executor appends *after* the final attempt returns).
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while target.attempts() < EXPECTED_ATTEMPTS && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        target.attempts(),
        EXPECTED_ATTEMPTS,
        "the retry executor must perform exactly RetryPolicy::max_attempts attempts \
         (default {EXPECTED_ATTEMPTS}: 1 initial + 2 retries); \
         a different count means RetryPolicy::default() drifted from its documented defaults"
    );
}
