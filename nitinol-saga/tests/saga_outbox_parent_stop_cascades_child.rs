//! When the parent saga stops, an in-flight outbox executor child must
//! cascade-stop with it.
//!
//! The outbox executor is a short-lived child
//! process spawned via `ctx.spawn_child(OutboxExecutorProcess { .. })`, so it
//! is registered in the saga's supervision tree.  The retry backoff lives in
//! the executor's custom Driver `next()`, which is multiplexed with the
//! system-signal receiver (biased `select!`), so a `Stop` cascaded from the
//! parent interrupts the executor **between attempts** rather than letting it
//! run the retry loop to completion.
//!
//! Observable contract: with a tell that fails every attempt, stopping the
//! saga while the executor is still in-flight (held at its first attempt, so
//! it has not yet exhausted retries) must prevent the executor from ever
//! appending a terminal marker.  The saga stream therefore retains its
//! `TellRequested` marker with **no** `TellAcked` and **no** `TellFailed`.
//!
//! Against a bare-`tokio::spawn` implementation this test fails: a detached
//! task is outside the supervision tree, so stopping the saga does not stop
//! it — it runs the full retry budget and appends `TellFailed`.

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
use tokio::sync::{Notify, Semaphore};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Decider, Decision, Event,
    SequenceCursor, TellError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::error::SendError;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// A TellTarget that fails every dispatch attempt.  The first attempt blocks on
// a semaphore gate until the test releases it; this lets the test deterministe
// reach the "executor is in-flight, retries not yet exhausted" state, issue the
// stop, and only then let the first attempt return.  Later attempts (if the
// stop has not cascaded yet) fail immediately so the executor is never wedged
// inside `apply()` — its only cancellation points are the backoff waits.

struct GatedFailingTarget<A: Aggregate> {
    attempts: Arc<AtomicUsize>,
    first_attempt_reached: Arc<Notify>,
    gate: Arc<Semaphore>,
    target_id: AggregateId,
    _phantom: PhantomData<fn() -> A>,
}

// Manual Clone — `derive(Clone)` would impose `A: Clone`, which `Aggregate`
// does not require.  Only the shared handles are cloned.
impl<A: Aggregate> Clone for GatedFailingTarget<A> {
    fn clone(&self) -> Self {
        Self {
            attempts: Arc::clone(&self.attempts),
            first_attempt_reached: Arc::clone(&self.first_attempt_reached),
            gate: Arc::clone(&self.gate),
            target_id: self.target_id.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> GatedFailingTarget<A> {
    fn new() -> Self {
        Self {
            attempts: Arc::new(AtomicUsize::new(0)),
            first_attempt_reached: Arc::new(Notify::new()),
            // 0 permits: the first attempt blocks until the test adds one.
            gate: Arc::new(Semaphore::new(0)),
            target_id: AggregateId::new("test-gated-failing-target"),
            _phantom: PhantomData,
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for GatedFailingTarget<A> {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        let attempts = Arc::clone(&self.attempts);
        let reached = Arc::clone(&self.first_attempt_reached);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                // Signal the test that the executor has begun its first
                // attempt, then block until the test releases the gate.
                reached.notify_one();
                gate.acquire()
                    .await
                    .expect("gate semaphore must stay open")
                    .forget();
            }
            Err(TellError::Send(SendError))
        })
    }

    fn aggregate_id(&self) -> &AggregateId {
        &self.target_id
    }
}

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("cascade_stop"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("cascade_stop"),
        TypeName::new("ReservationRequested"),
    );
}

#[derive(Default)]
struct Inventory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved;

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("cascade_stop"),
        TypeName::new("InventoryReserved"),
    );
}

impl Aggregate for Inventory {
    type Event = InventoryReserved;
    fn apply(&mut self, _event: InventoryReserved) {}
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Reserve;

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Reserve) -> Decision<InventoryReserved, (), Self::Rejection> {
        Decision::persist(vec![InventoryReserved]).output(())
    }
}

/// Correlation rule of [`CascadeSaga`]: one order process owns every
/// `OrderPlaced` this test publishes.
const CASCADE_SAGA_ID: &str = "cascade-saga-1";

struct CascadeSaga {
    target: GatedFailingTarget<Inventory>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for CascadeSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(CASCADE_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        // Persist a user event and a single tell.  The tell spawns the outbox
        // executor child; do NOT end the saga so the saga stays alive until the
        // test stops it explicitly.
        let effect = SagaEffect::persist(ReservationRequested { sku: event.sku })
            .combine(SagaEffect::tell(self.target.clone(), Reserve));
        self.handled.notify_one();
        Ok(effect)
    }
}

// Helpers

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

fn count_matching(events: &[LoadedEvent], pred: impl Fn(&OutboxKind) -> bool) -> usize {
    events
        .iter()
        .filter(|e| outbox_kind_of(e).is_some_and(|k| pred(&k)))
        .count()
}

// Test

/// Stopping the saga while its outbox executor is in-flight must cascade-stop
/// the executor (it is a supervision-tree child), so the executor never reaches
/// retry exhaustion and never appends a terminal marker.
#[tokio::test]
async fn parent_stop_cascades_to_in_flight_outbox_executor_child() {
    const MAX_ATTEMPTS: usize = 3; // RetryPolicy::default(): 1 initial + 2 retries.

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("cascade-order");
    append_order_placed(&upstream_store, &order_id, 1, "CASCADE-SKU").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(CASCADE_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let target = GatedFailingTarget::<Inventory>::new();
    let target_for_saga = target.clone();
    let handled_for_saga = Arc::clone(&handled);

    let saga_proxy =
        SagaProps::<CascadeSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            CascadeSaga {
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

    // The saga handles the upstream event and spawns the outbox executor.
    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Wait until the executor has begun its first attempt (and is now blocked
    // on the gate).  At this point the tell is in-flight but retries are not
    // exhausted, so no terminal marker can have been written.
    tokio::time::timeout(
        Duration::from_secs(3),
        target.first_attempt_reached.notified(),
    )
    .await
    .expect("outbox executor must begin its first attempt within 3 seconds");

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_matching(&events, |k| matches!(k, OutboxKind::TellRequested(_))),
        1,
        "exactly one TellRequested must have been written before the stop"
    );
    assert_eq!(
        count_matching(&events, |k| matches!(k, OutboxKind::TellAcked(_)))
            + count_matching(&events, |k| matches!(k, OutboxKind::TellFailed(_))),
        0,
        "no terminal marker may exist while the executor is still in-flight"
    );

    // Stop the saga.  This cascades a Stop down the supervision tree to the
    // in-flight executor child *before* we release its first attempt.
    saga_proxy.stop().await.expect("stop must be sent");

    // Give the cascade time to enqueue the Stop on the executor's system
    // channel while it is still blocked on the gate.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Release the first attempt.  It returns Err; the executor's lifecycle loop
    // then observes the already-queued Stop (biased over the retry Driver) and
    // terminates without performing the remaining attempts.
    target.gate.add_permits(1);

    // Allow well beyond the full retry budget (default backoff 100ms + 200ms).
    // A correctly cascaded child writes nothing further; a detached task would
    // exhaust retries and append TellFailed within this window.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_matching(&events, |k| matches!(k, OutboxKind::TellAcked(_))),
        0,
        "a cascade-stopped executor must not append TellAcked"
    );
    assert_eq!(
        count_matching(&events, |k| matches!(k, OutboxKind::TellFailed(_))),
        0,
        "a cascade-stopped executor must not run to retry exhaustion and append TellFailed; \
         its TellRequested marker stays open for the Replay path to recover"
    );
    assert!(
        target.attempts() < MAX_ATTEMPTS,
        "the executor must have been interrupted before exhausting its retry budget \
         (attempts={}, max={MAX_ATTEMPTS})",
        target.attempts()
    );
}
