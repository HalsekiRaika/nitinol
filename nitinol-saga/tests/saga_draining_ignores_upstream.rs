//! Regression: upstream events arriving while the saga lifecycle is
//! `Draining` must not reach `Saga::handle`.
//!
//! When `SagaEffect::End` is interpreted, the framework:
//!   1. Persists the durable `Ended` marker (restart guard).
//!   2. If in-flight tells remain, transitions to `Lifecycle::Draining` instead
//!      of stopping immediately.
//!
//! During `Draining`, the upstream subscription child is still alive (the stop
//! is deferred until the last tell settles).  Without a lifecycle guard, a
//! second upstream event delivered in this window would pass through to
//! `Saga::handle`, potentially producing new effects — violating the terminal
//! lifecycle contract.
//!
//! The guard: `Receive<EventEnvelope<_>>::recv` and
//! `Receive<FireScheduled>::recv` both check `Lifecycle::Draining` at entry
//! and return `Ok(())` immediately if set.
//!
//! Test strategy: pre-load the upstream store with TWO events before spawning
//! the saga.  The subscription child delivers them sequentially — message N+1
//! is enqueued only after message N's `recv` returns.  Therefore:
//!   recv(event-1) → handle → End → Ended written → Draining → return
//!   recv(event-2) → Draining guard → return immediately (no handle call)
//! This ordering is deterministic because actors process messages one at a time.

#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamDrainEvent {
    key: String,
}

impl Event for UpstreamDrainEvent {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("drain_guard"),
        TypeName::new("UpstreamDrainEvent"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DrainSagaEvent {
    key: String,
}

impl Event for DrainSagaEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("drain_guard"), TypeName::new("DrainSagaEvent"));
}

// Target aggregate (receives the tell from the saga)

#[derive(Default)]
struct DrainTargetAgg;

impl Aggregate for DrainTargetAgg {
    type Event = DrainSagaEvent;
    fn apply(&mut self, _event: DrainSagaEvent) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct DrainTargetCmd {
    key: String,
}

#[async_trait]
impl Decider<DrainTargetCmd> for DrainTargetAgg {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        cmd: DrainTargetCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<DrainSagaEvent>, Self::Rejection> {
        Ok(Effect::persist(DrainSagaEvent { key: cmd.key }))
    }
}

// Saga under test

/// On the first (and intended-only) upstream event: persist one saga event,
/// enqueue one tell, then end.  `handle_count` records every invocation so
/// the regression can assert it stayed at 1.
struct DrainGuardSaga {
    target: AggregateProxy<DrainTargetAgg>,
    handle_count: Arc<AtomicUsize>,
}

/// Correlation rule of [`DrainGuardSaga`]: both upstream events name the same
/// process instance — the guard, not correlation, is what must drop the second.
const DRAIN_GUARD_SAGA_ID: &str = "drain-guard-saga";

#[async_trait]
impl Saga for DrainGuardSaga {
    type SubscribedEvent = UpstreamDrainEvent;
    type Event = DrainSagaEvent;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(DRAIN_GUARD_SAGA_ID))
    }

    fn apply(&mut self, _event: DrainSagaEvent) {}

    async fn handle(
        &mut self,
        event: UpstreamDrainEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<DrainSagaEvent>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);

        let intent = TellIntent::new(
            self.target.clone(),
            DrainTargetCmd {
                key: event.key.clone(),
            },
        );

        // persist + one in-flight tell + end → enters Draining (not immediate stop)
        Ok(SagaEffect::persist(DrainSagaEvent { key: event.key })
            .with_tells(vec![intent])
            .then_end())
    }
}

// Helpers

async fn append_upstream_drain_event(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
    seq: u64,
    key: &str,
) {
    let payload = serde_json::to_vec(&UpstreamDrainEvent {
        key: key.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode UpstreamDrainEvent");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: seq,
                event_type: UpstreamDrainEvent::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append UpstreamDrainEvent");
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream")
        .try_collect()
        .await
        .expect("collect saga events")
}

async fn wait_for_ended_marker(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(store, saga_id).await;
        let has_ended = events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Ended(_))));
        if has_ended || std::time::Instant::now() >= deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Test

/// Regression: `Receive<EventEnvelope<_>>::recv` must return `Ok(())`
/// immediately when `Lifecycle::Draining`, so that a second upstream event
/// delivered before the in-flight tell settles is silently dropped.
#[tokio::test]
async fn draining_saga_ignores_upstream_event_before_tell_settles() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("drain-guard-agg");

    // Pre-load BOTH upstream events before spawning.  The subscription child
    // delivers them sequentially, so event-2 is enqueued only after event-1's
    // recv has returned (with Draining already set).
    append_upstream_drain_event(&upstream_store, &agg_id, 1, "evt-1").await;
    append_upstream_drain_event(&upstream_store, &agg_id, 2, "evt-2").await;

    let target_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let target_proxy = system
        .spawn_aggregate::<DrainTargetAgg>(
            AggregateId::new("drain-guard-target"),
            Arc::clone(&target_store),
        )
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(DRAIN_GUARD_SAGA_ID);
    let handle_count = Arc::new(AtomicUsize::new(0));

    let handle_count_clone = Arc::clone(&handle_count);
    let target_proxy_clone = target_proxy.clone();

    let _saga_proxy =
        SagaProps::<DrainGuardSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            DrainGuardSaga {
                target: target_proxy_clone.clone(),
                handle_count: Arc::clone(&handle_count_clone),
            }
        })
        .with_codec(system.codec::<DrainSagaEvent>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<UpstreamDrainEvent>(),
            SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // Wait until the durable Ended marker appears — confirms event-1 was
    // processed and the lifecycle entered Draining.
    let events_with_ended = wait_for_ended_marker(&saga_store, &saga_id).await;
    assert!(
        events_with_ended
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Ended(_)))),
        "Ended marker must be written after event-1 triggers End; \
         events: {:?}",
        events_with_ended
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );

    // Allow ample time for event-2 to be delivered and for the outbox executor
    // to settle (including any stop signal).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // --- Assertions ---

    // handle_count must be exactly 1.  event-2, which arrives while
    // Lifecycle::Draining is set, must be dropped by the guard without
    // calling Saga::handle.
    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "Saga::handle must be called exactly once; \
         if the Draining guard is missing, event-2 would also invoke handle \
         and bump the count to 2"
    );

    // Exactly one user (saga domain) event must exist in the saga stream.
    // A second handle call would append a second DrainSagaEvent, violating
    // the terminal lifecycle contract.
    let final_events = load_saga_events(&saga_store, &saga_id).await;
    let user_event_count = final_events
        .iter()
        .filter(|e| e.event_type == DrainSagaEvent::EVENT_TYPE)
        .count();
    assert_eq!(
        user_event_count,
        1,
        "exactly one user event must exist in the saga stream; \
         a missing Draining guard would allow event-2 to append a second \
         DrainSagaEvent; events: {:?}",
        final_events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
}
