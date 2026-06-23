//! Regression test for AI-52-008: when a saga returns
//! `Persist { tells }.then_end()`, the outbox executor must be able to
//! append the terminal marker (`TellAcked` or `TellFailed`) **before** the
//! saga process stops.
//!
//! Without the deferred-stop fix, `End` would call `stop_self()` immediately
//! via the priority system channel, so the outbox executor child's
//! `saga_proxy.tell(AppendTerminalAndClaim)` would fail with a closed-channel
//! error and no terminal marker would ever reach the store.

mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
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
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamEvent {
    key: String,
}

impl Event for UpstreamEvent {
    const EVENT_TYPE: EventType = EventType::from_str("persist_tell_end.UpstreamEvent");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaDomainEvent {
    key: String,
}

impl Event for SagaDomainEvent {
    const EVENT_TYPE: EventType = EventType::from_str("persist_tell_end.SagaDomainEvent");
}

// ---------------------------------------------------------------------------
// Target aggregate that the saga will tell
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TargetAggregate {
    received: u32,
}

impl Aggregate for TargetAggregate {
    type Event = SagaDomainEvent;

    fn apply(&mut self, _event: SagaDomainEvent) {
        self.received += 1;
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct TriggerCmd {
    key: String,
}

#[async_trait]
impl Decider<TriggerCmd> for TargetAggregate {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: TriggerCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<SagaDomainEvent>, Self::Rejection> {
        Ok(Effect::persist(SagaDomainEvent { key: cmd.key }))
    }
}

// ---------------------------------------------------------------------------
// Saga: on the first upstream event issue a tell then stop via End
// ---------------------------------------------------------------------------

struct TellThenEndSaga {
    target: AggregateProxy<TargetAggregate>,
    handled: Arc<Mutex<bool>>,
}

#[async_trait]
impl Saga for TellThenEndSaga {
    type SubscribedEvent = UpstreamEvent;
    type Event = SagaDomainEvent;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: SagaDomainEvent) {}

    async fn handle(
        &mut self,
        event: UpstreamEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaDomainEvent>, Self::Error> {
        *self.handled.lock().unwrap() = true;

        // Persist a tell AND then stop.  The key contract under test:
        // the outbox executor (spawned by the Persist branch) must be
        // able to append TellAcked/TellFailed *before* the End's
        // stop_self() closes the saga's mailbox.
        let effect =
            SagaEffect::tell(self.target.clone(), TriggerCmd { key: event.key }).then_end();

        Ok(effect)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn append_upstream(store: &Arc<dyn EventStore>, agg_id: &AggregateId, seq: u64, key: &str) {
    let payload = serde_json::to_vec(&UpstreamEvent {
        key: key.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode UpstreamEvent");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: seq,
                event_type: UpstreamEvent::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append UpstreamEvent");
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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// When `Persist { tells }.then_end()` is executed, the saga's `End` stop is
/// deferred until the outbox executor settles the terminal marker.  The store
/// must contain `TellRequested` followed by `TellAcked` (or `TellFailed`),
/// proving that the executor was not evicted before writing the terminal.
#[tokio::test]
async fn persist_with_tell_then_end_writes_terminal_marker_before_stop() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("persist-tell-end-agg");

    // Spawn the target aggregate — the saga will tell it via TriggerCmd.
    let target_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let target_proxy = system
        .spawn_aggregate::<TargetAggregate>(agg_id.clone(), Arc::clone(&target_store))
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("persist-tell-end-saga");
    let handled = Arc::new(Mutex::new(false));

    let handled_clone = Arc::clone(&handled);
    let target_proxy_clone = target_proxy.clone();
    let routed = saga_id.clone();

    let _saga_proxy =
        SagaProps::<TellThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TellThenEndSaga {
                target: target_proxy_clone.clone(),
                handled: Arc::clone(&handled_clone),
            }
        })
        .with_codec(system.codec::<SagaDomainEvent>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<UpstreamEvent>(),
            SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: 0,
            },
            move |_: &UpstreamEvent| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    // Publish the upstream event that triggers TellThenEndSaga::handle.
    append_upstream(&upstream_store, &agg_id, 1, "order-1").await;

    // Poll until the saga has handled the event.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if *handled.lock().unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for saga to handle the upstream event"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Now poll for the terminal marker.  Without the deferred-stop fix the
    // outbox executor would get a closed-channel error and never write
    // TellAcked/TellFailed, so this loop would time out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let terminal_count = events
            .iter()
            .filter(|e| {
                let t = e.event_type.as_str();
                t == "nitinol.saga.outbox.tell_acked" || t == "nitinol.saga.outbox.tell_failed"
            })
            .count();
        if terminal_count >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for outbox terminal marker; \
             outbox executor was evicted before End's stop was deferred; \
             events: {:?}",
            events
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let events = load_saga_events(&saga_store, &saga_id).await;
    let tell_requested = events
        .iter()
        .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_requested")
        .count();
    let tell_acked = events
        .iter()
        .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_acked")
        .count();
    let tell_failed = events
        .iter()
        .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_failed")
        .count();

    assert_eq!(
        tell_requested, 1,
        "saga stream must contain exactly one TellRequested marker"
    );
    assert_eq!(
        tell_acked + tell_failed,
        1,
        "saga stream must contain exactly one terminal marker (TellAcked or TellFailed); \
         got acked={tell_acked} failed={tell_failed}"
    );
}
