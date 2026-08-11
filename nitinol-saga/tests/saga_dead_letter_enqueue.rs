//! Saga DLQ: each saga failure kind, when it occurs, must be
//! enqueued as a `SagaPersisted::DeadLetter(DeadLetterEvent)` on the saga's
//! **own** EventStore stream, written on the wire under the reserved
//! type key `nitinol.saga.dead_letter` with a per-failure-kind variant:
//!
//! | SagaFailure                | variant (wire)                  | timing        |
//! |----------------------------|---------------------------------|---------------|
//! | HandleFailed               | `handle_failed`                 | immediate     |
//! | ScheduledFailed            | `scheduled_failed`              | immediate     |
//! | EndedSagaReceivedMessage   | `ended_saga_received_message`   | immediate     |
//! | TellFailed                 | `tell_failed`                   | after staged retry |
//! | PersistFailed              | `persist_failed`                | after staged retry |
//! | DecodeFailed               | `decode_failed`                 | immediate     |
//!
//! These tests observe the DLQ purely through its **public** contract: the
//! bytes that land on the saga stream.  They mirror the marker-observation
//! style already used by `saga_deadline_scheduler.rs` (schedule markers) and
//! `saga_outbox_tell_retry_failed.rs` (outbox markers) — no `pub(crate)`
//! internals are touched, so the wire contract is what is pinned.
//!
//! The default enqueue policy (all failures enqueued) is assumed here; the
//! policy override is covered in `saga_dead_letter_policy.rs`.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Context, Decider, Effect, Event,
    SequenceCursor, TellError,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::error::SendError;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent, TimerName};

// Self-contained JSON codec (kept local like saga_deadline_scheduler.rs).

#[derive(Default)]
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de> + 'static> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// Shared domain types

/// Upstream event that routes to the saga under test.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    tag: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_it"), TypeName::new("Ping"));
}

/// The saga's own domain event.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog {
    note: String,
}

impl Event for SagaLog {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_it"), TypeName::new("SagaLog"));
}

/// A concrete `Saga::Error` so `handle` / `on_scheduled` can fail on demand.
#[derive(Debug)]
struct SagaBoom(&'static str);

impl std::fmt::Display for SagaBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "saga boom: {}", self.0)
    }
}

impl std::error::Error for SagaBoom {}

// DLQ wire contract: reserved type key + per-arm variant.

/// Type-level identity of a dead-letter event: family `nitinol.saga`, type
/// name `dead_letter`.  Every per-failure-kind variant shares this type key.
const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

/// Type-level identity of an outbox marker — used to observe the `tell_failed`
/// outbox marker that precedes a TellFailed dead letter.
const OUTBOX_TYPE: EventType = EventType::new(Family::new("nitinol.saga"), TypeName::new("outbox"));

fn count_variant(events: &[LoadedEvent], type_key: EventType, variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == type_key.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
        .count()
}

fn count_domain(events: &[LoadedEvent], event_type: EventType) -> usize {
    events
        .iter()
        .filter(|e| e.event_type.type_key() == event_type.type_key())
        .count()
}

// Store helpers

async fn append_ping(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64, tag: &str) {
    let payload = serde_json::to_vec(&Ping {
        tag: tag.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode Ping must succeed");
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Ping must succeed");
}

/// Append an event with `Ping::EVENT_TYPE` but an invalid JSON payload so the
/// codec's `decode` call fails.  The `EVENT_TYPE` check in the upstream
/// transform passes; the subsequent decode fails — exercising the
/// `UpstreamMessage::DecodeFailed` path.
async fn append_corrupt_ping(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                // Corrupt bytes: valid UTF-8 but not valid JSON for `Ping`.
                payload: Bytes::from_static(b"NOT-VALID-JSON"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append corrupt Ping must succeed");
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

/// Poll the saga stream until at least one dead letter with `variant` is
/// present, or the timeout elapses.  Returns the full stream snapshot so the
/// caller can make additional assertions.
async fn wait_for_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    variant: &'static str,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_variant(&events, DEAD_LETTER_TYPE, variant) >= 1 {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for a `nitinol.saga.dead_letter.{variant}` event; \
                 saga stream held: {:?}",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Test 1 — HandleFailed → `handle_failed` (immediate)

struct HandleFailSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for HandleFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.handled.notify_one();
        Err(SagaBoom("handle"))
    }
}

/// Given a saga whose `handle` returns `Err`,
/// When an upstream event routes to it,
/// Then exactly one `handle_failed` dead letter is enqueued on the saga stream
/// and no domain event is persisted (the effect never materialised).
#[tokio::test]
async fn handle_error_enqueues_a_handle_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-handle-upstream", 1, "boom").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-handle-saga");
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<HandleFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            HandleFailSaga {
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-handle-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "handle_failed",
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(
        count_variant(&events, DEAD_LETTER_TYPE, "handle_failed"),
        1,
        "a single handle failure must enqueue exactly one `handle_failed` dead letter"
    );
    assert_eq!(
        count_domain(&events, SagaLog::EVENT_TYPE),
        0,
        "a failing handle must not persist any domain event"
    );
}

// Test 2 — ScheduledFailed → `scheduled_failed` (immediate)

struct ScheduleThenFailSaga;

#[async_trait]
impl Saga for ScheduleThenFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Ok(SagaEffect::schedule(
            TimerName::new("reminder"),
            Duration::from_millis(120),
            (),
        ))
    }

    async fn on_scheduled(
        &mut self,
        _message: (),
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Err(SagaBoom("scheduled"))
    }
}

/// Given a saga whose `on_scheduled` returns `Err`,
/// When the scheduled timer fires,
/// Then a `scheduled_failed` dead letter is enqueued on the saga stream.
#[tokio::test]
async fn on_scheduled_error_enqueues_a_scheduled_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-sched-upstream", 1, "tick").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-sched-saga");

    let routed = saga_id.clone();
    let _proxy = SagaProps::<ScheduleThenFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move || ScheduleThenFailSaga,
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: "dlq-sched-upstream".to_owned(),
            after: 0,
        },
        move |_event: &Ping| Some(routed.clone()),
    )
    .with_scheduler(scheduler.clone())
    .spawn(system.process_system())
    .await;

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "scheduled_failed",
        Duration::from_secs(6),
    )
    .await;

    assert!(
        count_variant(&events, DEAD_LETTER_TYPE, "scheduled_failed") >= 1,
        "a failing on_scheduled must enqueue a `scheduled_failed` dead letter"
    );
}

// Test 3 — TellFailed → `tell_failed` (after staged retry)

/// A tell target that fails every attempt and carries a known stream key so
/// the `SagaFailure::TellFailed::target` field can be asserted in tests.
struct FailingTellTarget<A: Aggregate> {
    /// Reported as `aggregate_id_str()` so `TellIntent` captures it and writes
    /// it into the `SagaFailure::TellFailed::target` dead-letter field.
    target_id: &'static str,
    _phantom: PhantomData<fn() -> A>,
}

impl<A: Aggregate> Clone for FailingTellTarget<A> {
    fn clone(&self) -> Self {
        Self {
            target_id: self.target_id,
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for FailingTellTarget<A> {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async move { Err(TellError::Send(SendError)) })
    }

    fn aggregate_id_str(&self) -> &str {
        self.target_id
    }
}

#[derive(Default)]
struct Inventory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved;

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("dlq_it"), TypeName::new("InventoryReserved"));
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

/// Known target ID for the TellFailed test — used to verify
/// `SagaFailure::TellFailed::target` is encoded and decoded correctly.
const TELL_FAIL_TARGET_ID: &str = "dlq-inventory-target";

struct TellFailSaga {
    target: FailingTellTarget<Inventory>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for TellFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let effect = SagaEffect::tell(self.target.clone(), Reserve);
        let full = SagaEffect::persist(SagaLog { note: event.tag }).combine(effect);
        self.handled.notify_one();
        Ok(full)
    }
}

/// Given a saga whose only tell fails on every dispatch attempt,
/// When the staged retry budget is exhausted,
/// Then a `tell_failed` dead letter is enqueued — and only alongside the
/// terminal `tell_failed` outbox marker, proving the DLQ entry is written
/// *after* staged retry, never on the first failed attempt.
///
/// Also verifies that `SagaFailure::TellFailed::target` carries the target
/// aggregate's stream key, not an internal tell_id.
#[tokio::test]
async fn tell_failing_every_attempt_enqueues_a_tell_failed_dead_letter_after_retry() {
    use nitinol_eventsource::SystemEvent;
    use nitinol_saga::DeadLetterEvent;
    use nitinol_saga::SagaFailure;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-tell-upstream", 1, "order").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-tell-saga");
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<TellFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TellFailSaga {
                target: FailingTellTarget {
                    target_id: TELL_FAIL_TARGET_ID,
                    _phantom: PhantomData,
                },
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-tell-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    let events =
        wait_for_dead_letter(&saga_store, &saga_id, "tell_failed", Duration::from_secs(8)).await;

    assert_eq!(
        count_variant(&events, DEAD_LETTER_TYPE, "tell_failed"),
        1,
        "an exhausted tell must enqueue exactly one `tell_failed` dead letter"
    );
    assert_eq!(
        count_variant(&events, OUTBOX_TYPE, "tell_failed"),
        1,
        "the DLQ entry must coexist with the terminal outbox `tell_failed` marker, \
         confirming enqueue happens after staged retry is exhausted"
    );

    // Verify `target` is the target aggregate's stream key — not an
    // internal tell_id numeric placeholder.
    let tell_failed_event = events
        .iter()
        .find(|e| {
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new("tell_failed"))
        })
        .expect("tell_failed dead letter must exist");
    let decoded = DeadLetterEvent::decode(&tell_failed_event.payload)
        .expect("tell_failed dead letter must decode");
    match decoded.failure {
        SagaFailure::TellFailed { target, .. } => {
            assert_eq!(
                target.as_str(),
                TELL_FAIL_TARGET_ID,
                "TellFailed::target must be the target aggregate stream key"
            );
        }
        other => panic!("expected TellFailed, got {:?}", other),
    }
}

// Test 4 — EndedSagaReceivedMessage → `ended_saga_received_message` (immediate)

#[derive(Default)]
struct DrainTargetAgg;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DrainDone;

impl Event for DrainDone {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_it"), TypeName::new("DrainDone"));
}

impl Aggregate for DrainTargetAgg {
    type Event = DrainDone;
    fn apply(&mut self, _event: DrainDone) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct DrainCmd;

#[async_trait]
impl Decider<DrainCmd> for DrainTargetAgg {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        _cmd: DrainCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<DrainDone>, Self::Rejection> {
        Ok(Effect::persist(DrainDone))
    }
}

/// A tell target for Test 4 that blocks until a `Notify` is triggered.
///
/// With `ask()`-based delivery, the DirectPoller
/// only dispatches event-2 after event-1's `recv()` returns.  In a
/// multi-threaded Tokio runtime the OutboxExecutor (spawned during event-1
/// handling) can complete its round-trip to the tell target before the
/// DirectPoller sends event-2, making `ready_to_stop()` return true before
/// the saga ever sees event-2.
///
/// This target prevents that race by blocking the OutboxExecutor's `tell()`
/// until the test has confirmed the `ended_saga_received_message` DLQ entry is
/// present, at which point it calls `notify_one()` to release the executor.
#[derive(Clone)]
struct BlockingDrainTellTarget {
    notify: Arc<Notify>,
}

impl AggregateTellTarget<DrainTargetAgg> for BlockingDrainTellTarget {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        DrainTargetAgg: Decider<C>,
        C: Send + Sync + 'static,
    {
        let notify = Arc::clone(&self.notify);
        Box::pin(async move {
            notify.notified().await;
            Ok(())
        })
    }

    fn aggregate_id_str(&self) -> &str {
        "dlq-ended-target"
    }
}

struct EndOnFirstSaga {
    target: BlockingDrainTellTarget,
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for EndOnFirstSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        // persist + one in-flight tell + end → enters Draining (stays alive to
        // receive and drop the second upstream event).
        let intent = TellIntent::new(self.target.clone(), DrainCmd);
        Ok(SagaEffect::persist(SagaLog { note: event.tag })
            .with_tells(vec![intent])
            .then_end())
    }
}

/// Given a saga that has ended (Draining after `then_end`),
/// When a further upstream event is delivered to it,
/// Then that message is dropped **and** recorded as an
/// `ended_saga_received_message` dead letter, while `handle` is never invoked
/// a second time.
#[tokio::test]
async fn message_to_ended_saga_enqueues_an_ended_saga_received_message_dead_letter() {
    use nitinol_eventsource::SystemEvent;
    use nitinol_saga::{DeadLetterEvent, SagaFailure};

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // Pre-load BOTH events.  The DirectPoller (via `poll_direct_once`) reads the
    // entire batch in one `store.load()` call and delivers event-1 then event-2
    // sequentially within the same loop using `ask()`.  Event-2 is
    // therefore dispatched immediately after event-1's `ask()` resolves — there
    // is no inter-poll-tick gap.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-ended-upstream", 1, "first").await;
    append_ping(&upstream_store, "dlq-ended-upstream", 2, "second").await;

    // Capture the expected raw payload of the second (dropped) ping so we can
    // assert it is preserved in the dead letter.
    let expected_message_payload = serde_json::to_vec(&Ping {
        tag: "second".to_owned(),
    })
    .map(Bytes::from)
    .expect("encode second Ping must succeed");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-ended-saga");
    let handle_count = Arc::new(AtomicUsize::new(0));
    // The OutboxExecutor (spawned during event-1 handling) runs concurrently on
    // the Tokio thread pool.  In a multi-threaded runtime it can complete the
    // DrainCmd tell before the DirectPoller delivers event-2, which would cause
    // `ready_to_stop()` to fire and the saga to stop before event-2 arrives.
    // BlockingDrainTellTarget prevents this race: the OutboxExecutor's `tell()`
    // blocks on `drain_unblock.notified()` until the test has confirmed the DLQ
    // entry, at which point the test calls `notify_one()` to release it.
    let drain_unblock = Arc::new(Notify::new());

    let handle_count_for_saga = Arc::clone(&handle_count);
    let drain_unblock_for_saga = Arc::clone(&drain_unblock);
    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<EndOnFirstSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            EndOnFirstSaga {
                target: BlockingDrainTellTarget {
                    notify: Arc::clone(&drain_unblock_for_saga),
                },
                handle_count: Arc::clone(&handle_count_for_saga),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-ended-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "ended_saga_received_message",
        Duration::from_secs(6),
    )
    .await;

    // DLQ confirmed — release the OutboxExecutor so the saga can drain cleanly.
    drain_unblock.notify_one();

    assert!(
        count_variant(&events, DEAD_LETTER_TYPE, "ended_saga_received_message") >= 1,
        "an upstream message delivered after the saga ended must enqueue an \
         `ended_saga_received_message` dead letter"
    );
    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "the post-end message must be dropped, not delivered to Saga::handle"
    );
    assert_eq!(
        count_domain(&events, SagaLog::EVENT_TYPE),
        1,
        "only the first event may persist a domain event; the dropped message must not"
    );

    // The dead letter must carry the raw wire payload of the dropped
    // upstream message, not an empty placeholder.
    let ended_event = events
        .iter()
        .find(|e| {
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new("ended_saga_received_message"))
        })
        .expect("ended_saga_received_message dead letter must exist");
    let decoded = DeadLetterEvent::decode(&ended_event.payload)
        .expect("ended_saga_received_message dead letter must decode");
    match decoded.failure {
        SagaFailure::EndedSagaReceivedMessage { message } => {
            assert_eq!(
                message, expected_message_payload,
                "EndedSagaReceivedMessage::message must carry the raw payload of the \
                 dropped upstream event"
            );
        }
        other => panic!("expected EndedSagaReceivedMessage, got {:?}", other),
    }
}

// Test 5 — PersistFailed → `persist_failed` (after staged retry)

/// An `EventStore` that fails the first `fail_count` `append` calls, then
/// routes subsequent appends (and all loads) to a backing `InMemoryEventStore`.
///
/// - `fail_count = 1`: first attempt fails, first retry succeeds → no DLQ
/// - `fail_count = 3`: all three default attempts fail, DLQ enqueued on 4th call
struct FailNAppendStore {
    inner: InMemoryEventStore,
    remaining_failures: AtomicUsize,
}

impl FailNAppendStore {
    fn new(fail_count: usize) -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            remaining_failures: AtomicUsize::new(fail_count),
        }
    }
}

#[async_trait]
impl EventStore for FailNAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.remaining_failures.load(Ordering::SeqCst) > 0 {
            self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
            Err(AppendError::Backend("injected persist failure".into()))
        } else {
            self.inner.append(key, events).await
        }
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

struct PersistFailSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.handled.notify_one();
        Ok(SagaEffect::persist(SagaLog { note: event.tag }))
    }
}

/// Given a saga whose EventStore succeeds on the **first retry** of a failed
/// persist batch (only the first of three attempts fails),
/// When a Persist effect is attempted,
/// Then the domain event is persisted on retry and **no DLQ entry is recorded**
/// — proving that PersistFailed obeys staged retry: the DLQ entry is only
/// enqueued after all retry attempts are exhausted.
#[tokio::test]
async fn persist_retry_recovers_domain_event_without_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(
        &upstream_store,
        "dlq-persist-retry-ok-upstream",
        1,
        "recover-test",
    )
    .await;

    // Fails only the first append; the first retry (attempt 2) succeeds.
    let saga_store: Arc<dyn EventStore> = Arc::new(FailNAppendStore::new(1));
    let saga_id = SagaId::new("dlq-persist-retry-ok-saga");
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<PersistFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            PersistFailSaga {
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-persist-retry-ok-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Give the saga time to complete the retry and settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        count_variant(&events, DEAD_LETTER_TYPE, "persist_failed"),
        0,
        "a persist failure recovered on the first retry must NOT enqueue a \
         `persist_failed` dead letter (DLQ only after staged retry is exhausted)"
    );
    assert_eq!(
        count_domain(&events, SagaLog::EVENT_TYPE),
        1,
        "the domain event must be persisted when the first retry succeeds"
    );
}

/// Given a saga whose EventStore fails **all** staged retry attempts for a
/// domain-event persist batch (default budget: 3 attempts),
/// When a Persist effect is attempted,
/// Then a `persist_failed` dead letter is appended to the saga stream only
/// after all attempts are exhausted.
#[tokio::test]
async fn persist_failure_enqueues_a_persist_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-persist-upstream", 1, "persist-test").await;

    // Fails the first 3 appends (= default RetryPolicy::max_attempts).  The 4th
    // call (the DLQ enqueue) is routed to the inner store and succeeds, so the
    // dead letter is observable in the saga stream.
    let saga_store: Arc<dyn EventStore> = Arc::new(FailNAppendStore::new(3));
    let saga_id = SagaId::new("dlq-persist-saga");
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<PersistFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            PersistFailSaga {
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-persist-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "persist_failed",
        Duration::from_secs(5),
    )
    .await;

    assert_eq!(
        count_variant(&events, DEAD_LETTER_TYPE, "persist_failed"),
        1,
        "exhausting all staged persist retries must enqueue exactly one \
         `persist_failed` dead letter"
    );
    assert_eq!(
        count_domain(&events, SagaLog::EVENT_TYPE),
        0,
        "the domain event must not appear in the saga stream when all persist retries failed"
    );
}

// Test 6 — DecodeFailed → `decode_failed` (immediate)

/// A scheduled-message type whose `Deserialize` always fails regardless of
/// what bytes it receives.  The `Serialize` implementation produces valid JSON
/// (`null`) so `SagaEffect::schedule` can store the payload durably; the
/// decode failure is injected only at the timer-fire path.
struct AlwaysDecodeFails;

impl serde::Serialize for AlwaysDecodeFails {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_unit()
    }
}

impl<'de> serde::Deserialize<'de> for AlwaysDecodeFails {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Consume the entire value so the deserializer is not left with
        // unconsumed input, then unconditionally signal failure.
        let _ = <serde::de::IgnoredAny as serde::Deserialize<'de>>::deserialize(deserializer)?;
        Err(serde::de::Error::custom(
            "intentional decode failure for DLQ test",
        ))
    }
}

struct DecodeFailSaga;

#[async_trait]
impl Saga for DecodeFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = std::convert::Infallible;
    type ScheduledMessage = AlwaysDecodeFails;

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        // Schedule a timer; the stored payload deserialises to `null`, which
        // `AlwaysDecodeFails::deserialize` consumes then rejects.
        Ok(SagaEffect::schedule(
            TimerName::new("decode-test"),
            Duration::from_millis(80),
            AlwaysDecodeFails,
        ))
    }

    async fn on_scheduled(
        &mut self,
        _msg: AlwaysDecodeFails,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        // Never reached — the deserialise step fails before this is called.
        Ok(SagaEffect::None)
    }
}

/// Given a saga whose `ScheduledMessage` type always fails to deserialise,
/// When the scheduler fires a timer for that saga,
/// Then a `decode_failed` dead letter is enqueued on the saga stream
/// immediately.
#[tokio::test]
async fn decode_failure_in_scheduled_payload_enqueues_a_decode_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-decode-upstream", 1, "decode-test").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-decode-saga");

    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<DecodeFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            DecodeFailSaga
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-decode-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "decode_failed",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        count_variant(&events, DEAD_LETTER_TYPE, "decode_failed") >= 1,
        "a scheduled-payload decode failure must enqueue a `decode_failed` dead letter"
    );
}

// Test 7 — upstream message decode failure → `decode_failed` (immediate)

/// A minimal saga that accepts Ping events and returns no effect.  Used to
/// verify the `UpstreamMessage::DecodeFailed` DLQ path without scheduling or
/// error injection in the handler itself.
struct UpstreamDecodeTestSaga;

#[async_trait]
impl Saga for UpstreamDecodeTestSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        // Must not be called for the corrupt event; returns None for any valid Ping.
        Ok(SagaEffect::None)
    }
}

/// Given an upstream stream carrying an event with the correct `EVENT_TYPE`
/// (`Ping::EVENT_TYPE`) but an invalid JSON payload,
/// When a saga subscribes to that stream,
/// Then the transform decode failure is forwarded to the saga as a
/// `SagaFailure::DecodeFailed` dead letter, immediately, on the
/// saga's own stream — not silently discarded.
#[tokio::test]
async fn upstream_message_decode_failure_enqueues_a_decode_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // Append a corrupt payload that carries Ping's EVENT_TYPE but is not valid
    // JSON for `Ping` — the type-key filter in the transform passes, then
    // codec.decode() fails, triggering the DecodeFailed path.
    append_corrupt_ping(&upstream_store, "dlq-upstream-decode-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-upstream-decode-saga");

    let routed = saga_id.clone();
    let _proxy =
        SagaProps::<UpstreamDecodeTestSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || {
            UpstreamDecodeTestSaga
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "dlq-upstream-decode-upstream".to_owned(),
                after: 0,
            },
            move |_event: &Ping| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    let events = wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "decode_failed",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        count_variant(&events, DEAD_LETTER_TYPE, "decode_failed") >= 1,
        "an upstream message with an invalid payload must enqueue a `decode_failed` dead letter"
    );
}

// Test 8 — decode_failure_route_fn: only the routing target records DLQ

/// Two sagas subscribe to the **same** upstream stream, which carries a
/// corrupt event.  Both configure `decode_failure_route_fn` pointing to
/// saga-A.  Only saga-A records a `decode_failed` dead letter; saga-B routes
/// the failure to saga-A and, since that is not saga-B, opts out silently.
///
/// This verifies that when multiple sagas subscribe
/// to the same upstream, a single corrupt event does not produce duplicate DLQ
/// entries across every subscriber.
#[tokio::test]
async fn decode_failure_route_fn_prevents_non_target_saga_from_recording_dead_letter() {
    use nitinol_persistence::AggregateId;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // Single corrupt event on a shared upstream stream.
    append_corrupt_ping(&upstream_store, "dlq-route-shared-upstream", 1).await;

    let saga_a_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_b_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id_a = SagaId::new("dlq-route-saga-a");
    let saga_id_b = SagaId::new("dlq-route-saga-b");

    // Saga A: route function returns Some(saga_id_a) for any decode failure.
    // Since saga_id_a == self.saga_id, saga-A records the DLQ entry.
    let routed_a = saga_id_a.clone();
    let route_target_a = saga_id_a.clone();
    let _proxy_a = SagaProps::<UpstreamDecodeTestSaga>::new(
        saga_id_a.clone(),
        Arc::clone(&saga_a_store),
        || UpstreamDecodeTestSaga,
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: "dlq-route-shared-upstream".to_owned(),
            after: 0,
        },
        move |_event: &Ping| Some(routed_a.clone()),
    )
    .with_decode_failure_route(move |_: &AggregateId, _: u64| Some(route_target_a.clone()))
    .spawn(system.process_system())
    .await;

    // Saga B: route function also returns Some(saga_id_a) — pointing at saga-A.
    // Since saga_id_a ≠ self.saga_id (= saga_id_b), saga-B opts out and records
    // no DLQ entry.
    let routed_b = saga_id_b.clone();
    let route_target_b = saga_id_a.clone();
    let _proxy_b = SagaProps::<UpstreamDecodeTestSaga>::new(
        saga_id_b.clone(),
        Arc::clone(&saga_b_store),
        || UpstreamDecodeTestSaga,
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: "dlq-route-shared-upstream".to_owned(),
            after: 0,
        },
        move |_event: &Ping| Some(routed_b.clone()),
    )
    .with_decode_failure_route(move |_: &AggregateId, _: u64| Some(route_target_b.clone()))
    .spawn(system.process_system())
    .await;

    // Saga A must record a DLQ entry for the corrupt event.
    let events_a = wait_for_dead_letter(
        &saga_a_store,
        &saga_id_a,
        "decode_failed",
        Duration::from_secs(5),
    )
    .await;

    assert!(
        count_variant(&events_a, DEAD_LETTER_TYPE, "decode_failed") >= 1,
        "saga-A (routing target) must record a `decode_failed` dead letter \
         for the corrupt upstream event"
    );

    // Give saga B ample time to process (or skip) the corrupt event.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Saga B must NOT record a DLQ entry.
    let events_b = load_saga_events(&saga_b_store, &saga_id_b).await;
    assert_eq!(
        count_variant(&events_b, DEAD_LETTER_TYPE, "decode_failed"),
        0,
        "saga-B must NOT record a `decode_failed` dead letter — its \
         decode_failure_route_fn routes this failure to saga-A, which is not \
         saga-B"
    );
}
