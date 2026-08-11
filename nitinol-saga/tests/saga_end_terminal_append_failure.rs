//! Regression test: when the outbox executor's terminal-marker append fails,
//! a saga that returned `then_end()` must still stop.
//!
//! Without the fix the deferred-stop condition was only triggered when the
//! terminal append succeeded.  A store failure left the saga stuck in
//! `Lifecycle::Draining` with in-flight entries remaining in `tell_states`
//! forever — the saga would never receive subsequent upstream events and
//! silently stall.
//!
//! With the fix, the outbox executor child sends `OutboxReport` to
//! the parent saga via fire-and-forget `tell` regardless of the append outcome.
//! On append failure, the `OutboxReport` handler transitions the entry from
//! `Pending` to `AppendFailed` in `tell_states`; `AppendFailed` is not counted
//! as in-flight, so `ready_to_stop()` fires when `Lifecycle::Draining` and no
//! `Pending` entries remain.
//!
//! The test verifies: after `tell(...).then_end()` where every terminal append
//! fails, the saga process stops and no further upstream events are processed.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    SequenceCursor,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamTrigger {
    key: String,
}

impl Event for UpstreamTrigger {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("term_append_fail"),
        TypeName::new("UpstreamTrigger"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaMarker {
    key: String,
}

impl Event for SagaMarker {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("term_append_fail"), TypeName::new("SagaMarker"));
}

// Target aggregate

#[derive(Default)]
struct TargetAgg;

impl Aggregate for TargetAgg {
    type Event = SagaMarker;
    fn apply(&mut self, _event: SagaMarker) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct TargetCmd {
    key: String,
}

#[async_trait]
impl Decider<TargetCmd> for TargetAgg {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        cmd: TargetCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<SagaMarker>, Self::Rejection> {
        Ok(Effect::persist(SagaMarker { key: cmd.key }))
    }
}

// Saga: issues a tell then calls End on the first upstream event

struct TellThenEndSaga {
    target: AggregateProxy<TargetAgg>,
    /// Counts how many times `handle` has been called.
    handle_count: Arc<Mutex<usize>>,
    /// Notified after the first handle call so the test can advance.
    handled_first: Arc<Notify>,
}

#[async_trait]
impl Saga for TellThenEndSaga {
    type SubscribedEvent = UpstreamTrigger;
    type Event = SagaMarker;
    type State = ();
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: SagaMarker) {}

    async fn handle(
        &mut self,
        event: UpstreamTrigger,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaMarker>, Self::Error> {
        *self.handle_count.lock().expect(
            "handle_count mutex is never poisoned: no holder panics while the guard is alive",
        ) += 1;
        self.handled_first.notify_one();
        // Tell the target and immediately end.  The terminal append for the
        // TellRequested outbox marker will fail (injected via the saga store).
        Ok(SagaEffect::tell(self.target.clone(), TargetCmd { key: event.key }).then_end())
    }
}

// Saga store that allows the first two appends to succeed:
//   attempt 0 — the atomic Persist batch (TellRequested marker)
//   attempt 1 — the durable Ended marker written by the End interpreter
// All subsequent appends (TellAcked / TellFailed terminal markers) fail.
// This lets the saga reach Draining normally; the test then verifies it
// stops even though the terminal-marker appends fail.

struct TwoSuccessStore {
    append_count: AtomicUsize,
    inner: InMemoryEventStore,
}

impl TwoSuccessStore {
    fn new() -> Self {
        Self {
            append_count: AtomicUsize::new(0),
            inner: InMemoryEventStore::default(),
        }
    }
}

#[async_trait]
impl EventStore for TwoSuccessStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let attempt = self.append_count.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            // Attempts 0 and 1 succeed so that the TellRequested batch and the
            // durable Ended marker are both durably written.
            self.inner.append(key, events).await
        } else {
            // Subsequent appends: TellAcked / TellFailed terminal markers — fail.
            Err(AppendError::Backend(
                "injected terminal append failure".into(),
            ))
        }
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Helpers

async fn append_upstream(store: &Arc<dyn EventStore>, agg_id: &AggregateId, seq: u64, key: &str) {
    let payload = serde_json::to_vec(&UpstreamTrigger {
        key: key.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode UpstreamTrigger");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: seq,
                event_type: UpstreamTrigger::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append UpstreamTrigger");
}

// Test

/// After `tell(...).then_end()` where every terminal-marker append fails, the
/// saga must still stop and must not process any subsequent upstream events.
#[tokio::test]
async fn saga_stops_after_end_even_when_terminal_append_fails() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new("term-fail-agg");

    let target_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let target_proxy = system
        .spawn_aggregate::<TargetAgg>(agg_id.clone(), Arc::clone(&target_store))
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(TwoSuccessStore::new());
    let saga_id = SagaId::new("term-fail-saga");

    let handle_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let handled_first = Arc::new(Notify::new());

    let handle_count_clone = Arc::clone(&handle_count);
    let handled_first_clone = Arc::clone(&handled_first);
    let target_proxy_clone = target_proxy.clone();
    let routed = saga_id.clone();

    let _saga_proxy =
        SagaProps::<TellThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TellThenEndSaga {
                target: target_proxy_clone.clone(),
                handle_count: Arc::clone(&handle_count_clone),
                handled_first: Arc::clone(&handled_first_clone),
            }
        })
        .with_codec(system.codec::<SagaMarker>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<UpstreamTrigger>(),
            SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: 0,
            },
            move |_: &UpstreamTrigger| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    // Push the first upstream event to trigger handle().
    append_upstream(&upstream_store, &agg_id, 1, "first").await;

    tokio::time::timeout(Duration::from_secs(5), handled_first.notified())
        .await
        .expect("saga must handle the first upstream event within 5 seconds");

    // Give the outbox executor child time to send OutboxReport and
    // the deferred stop to fire.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Push a second upstream event.  If the saga stopped correctly, handle()
    // must not be invoked for it.
    append_upstream(&upstream_store, &agg_id, 2, "second").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let final_count = *handle_count
        .lock()
        .expect("handle_count mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        final_count, 1,
        "saga must have stopped after End even though terminal append failed; \
         Saga::handle must not be called for the second upstream event, \
         but handle_count = {final_count}"
    );
}
