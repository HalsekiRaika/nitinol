//! Saga DLQ: each saga failure kind, when it occurs, must be enqueued as a
//! `DeadLetterEvent` on the saga's **own** EventStore stream, written on the
//! wire under the reserved type `nitinol.saga.dead_letter`, with the failure
//! kind carried by the payload the record holds:
//!
//! | SagaFailure                | payload `oneof` arm             | timing        |
//! |----------------------------|---------------------------------|---------------|
//! | HandleFailed               | `handle_failed`                 | immediate     |
//! | ScheduledFailed            | `scheduled_failed`              | immediate     |
//! | EndedSagaReceivedMessage   | `ended_saga_received_message`   | immediate     |
//! | TellFailed                 | `tell_failed`                   | after staged retry |
//! | PersistFailed              | `persist_failed`                | after staged retry |
//! | DecodeFailed               | `decode_failed`                 | immediate     |
//!
//! `ScheduledFailed` names the entry the input arrived at, not a method: a
//! deadline that the deadline arm of `Saga::handle` could not handle is recorded
//! under that kind, so a policy written against it keeps meaning the same thing.
//! `DecodeFailed` likewise covers a deadline firing whose persisted payload
//! cannot be read back through the codec its message type named.  An undecodable
//! *upstream* record is deliberately absent from this table: it carries no typed
//! event for `Saga::correlate` to name an owner from, so it is reported and
//! skipped rather than enqueued — pinned by
//! `saga_upstream_decode_failure_is_skipped.rs`.
//!
//! These tests observe the DLQ purely through its **public** contract: the
//! bytes that land on the saga stream.  They mirror the marker-observation
//! style already used by `saga_deadline_scheduler.rs` (schedule markers) and
//! `saga_outbox_tell_retry_failed.rs` (outbox markers) — no `pub(crate)`
//! internals are touched, so the wire contract is what is pinned.
//!
//! The default enqueue policy (all failures enqueued) is assumed here; the
//! policy override is covered in `saga_dead_letter_policy.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::{dead_letter_of, failure_kind, outbox_kind_of, OutboxKind};

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
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Decider, Decision, Event, TellError,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::error::SendError;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe,
    Subscription, TellIntent, TimerName,
};

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
    type Codec = JsonCodec;
}

/// The saga's own domain event.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog {
    note: String,
}

impl Event for SagaLog {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_it"), TypeName::new("SagaLog"));
    type Codec = JsonCodec;
}

/// The alphabet of the sagas here that have no time of their own: one observed
/// source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Ping(Ping),
}

/// A concrete `Saga::Error` so the handler can fail on demand.
#[derive(Debug)]
struct SagaBoom(&'static str);

impl std::fmt::Display for SagaBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "saga boom: {}", self.0)
    }
}

impl std::error::Error for SagaBoom {}

// Correlation rules.  Each saga type below drives exactly one process instance,
// so its rule is a single constant that both `Saga::correlate` and the spawn
// site naming that instance refer to.

const HANDLE_FAIL_SAGA_ID: &str = "dlq-handle-saga";
const SCHEDULE_THEN_FAIL_SAGA_ID: &str = "dlq-sched-saga";
const TELL_FAIL_SAGA_ID: &str = "dlq-tell-saga";
const END_ON_FIRST_SAGA_ID: &str = "dlq-ended-saga";
const PERSIST_FAIL_SAGA_ID: &str = "dlq-persist-saga";
const DECODE_FAIL_SAGA_ID: &str = "dlq-decode-saga";

/// Dead letters on this stream recording the named failure.
///
/// Every dead letter carries the one `nitinol.saga.dead_letter` path, so which
/// failure a record holds is read from the payload it carries.
fn count_dead_letter_kind(events: &[LoadedEvent], kind: &'static str) -> usize {
    dead_letters(events)
        .filter(|record| failure_kind(&record.failure) == kind)
        .count()
}

fn dead_letters(
    events: &[LoadedEvent],
) -> impl Iterator<Item = nitinol_saga::DeadLetterEvent> + '_ {
    events.iter().filter_map(dead_letter_of)
}

/// Terminal `tell_failed` outbox markers on this stream.
fn count_outbox_tell_failed(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter_map(outbox_kind_of)
        .filter(|kind| matches!(kind, OutboxKind::TellFailed(_)))
        .count()
}

fn count_domain(events: &[LoadedEvent], event_type: EventType) -> usize {
    events.iter().filter(|e| e.event_type == event_type).count()
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Poll the saga stream until at least one dead letter recording `kind` is
/// present, or the timeout elapses.  Returns the full stream snapshot so the
/// caller can make additional assertions.
async fn wait_for_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    kind: &'static str,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_dead_letter_kind(&events, kind) >= 1 {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for a `nitinol.saga.dead_letter` record of {kind:?}; \
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
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = SagaBoom;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(HANDLE_FAIL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
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
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-handle-upstream", 1, "boom").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(HANDLE_FAIL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let _proxy = SagaProps::<HandleFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| HandleFailSaga {
            handled: Arc::clone(&handled_for_saga),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(&upstream_store, "dlq-handle-upstream"))
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
        count_dead_letter_kind(&events, "handle_failed"),
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

/// The deadline message of [`ScheduleThenFailSaga`] — a value persisted inside
/// the schedule fact, so it names the codec that writes it down.
struct Reminder;

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from_static(b"reminder"))
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    type Codec = ReminderCodec;
}

/// The alphabet of [`ScheduleThenFailSaga`]: one observed source, plus its own
/// time — the deadline whose arm fails.
// The observed payload is decoded and delivered as the variant it belongs to;
// what this saga decides does not read it, which `dead_code` cannot tell apart
// from a field nothing fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum ScheduledInput {
    Ping(Ping),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

struct ScheduleThenFailSaga {
    deadline: Deadline<Reminder>,
}

#[async_trait]
impl Saga for ScheduleThenFailSaga {
    type Subscribe = ScheduledInput;
    type Event = SagaLog;
    type Error = SagaBoom;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduledInput::Ping(_) => Some(SagaId::new(SCHEDULE_THEN_FAIL_SAGA_ID)),
            // A firing is addressed to the instance that set it and never
            // travels the subscription path.
            ScheduledInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        match input {
            ScheduledInput::Ping(_) => Ok(self.deadline.set(
                TimerName::new("reminder"),
                Duration::from_millis(120),
                Reminder,
            )),
            ScheduledInput::Reminder(_) => Err(SagaBoom("scheduled")),
        }
    }
}

/// Given a saga whose deadline arm returns `Err`,
/// When the deadline it set fires,
/// Then a `scheduled_failed` dead letter is enqueued on the saga stream.
#[tokio::test]
async fn a_failing_deadline_arm_enqueues_a_scheduled_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-sched-upstream", 1, "tick").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SCHEDULE_THEN_FAIL_SAGA_ID);

    let _proxy = SagaProps::<ScheduleThenFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| ScheduleThenFailSaga {
            deadline: components.deadline(),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(&upstream_store, "dlq-sched-upstream"))
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
        count_dead_letter_kind(&events, "scheduled_failed") >= 1,
        "a failing deadline arm must enqueue a `scheduled_failed` dead letter"
    );
}

// Test 3 — TellFailed → `tell_failed` (after staged retry)

/// A tell target that fails every attempt and carries a known stream key so
/// the `SagaFailure::TellFailed::target` field can be asserted in tests.
struct FailingTellTarget<A: Aggregate> {
    /// Reported as `aggregate_id()` so `TellIntent` captures it and writes
    /// it into the `SagaFailure::TellFailed::target` dead-letter field.
    target_id: AggregateId,
    _phantom: PhantomData<fn() -> A>,
}

impl<A: Aggregate> Clone for FailingTellTarget<A> {
    fn clone(&self) -> Self {
        Self {
            target_id: self.target_id.clone(),
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

    fn aggregate_id(&self) -> &AggregateId {
        &self.target_id
    }
}

#[derive(Default)]
struct Inventory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved;

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("dlq_it"), TypeName::new("InventoryReserved"));
    type Codec = JsonCodec;
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

/// Known target ID for the TellFailed test — used to verify
/// `SagaFailure::TellFailed::target` is encoded and decoded correctly.
const TELL_FAIL_TARGET_ID: &str = "dlq-inventory-target";

struct TellFailSaga {
    target: FailingTellTarget<Inventory>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for TellFailSaga {
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(TELL_FAIL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let Observed::Ping(event) = input;
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
    use nitinol_saga::SagaFailure;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-tell-upstream", 1, "order").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TELL_FAIL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let _proxy = SagaProps::<TellFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| TellFailSaga {
            target: FailingTellTarget {
                target_id: AggregateId::new(TELL_FAIL_TARGET_ID),
                _phantom: PhantomData,
            },
            handled: Arc::clone(&handled_for_saga),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(&upstream_store, "dlq-tell-upstream"))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    let events =
        wait_for_dead_letter(&saga_store, &saga_id, "tell_failed", Duration::from_secs(8)).await;

    assert_eq!(
        count_dead_letter_kind(&events, "tell_failed"),
        1,
        "an exhausted tell must enqueue exactly one `tell_failed` dead letter"
    );
    assert_eq!(
        count_outbox_tell_failed(&events),
        1,
        "the DLQ entry must coexist with the terminal outbox `tell_failed` marker, \
         confirming enqueue happens after staged retry is exhausted"
    );

    // Verify `target` is the target aggregate's stream key — not an
    // internal tell_id numeric placeholder.
    let decoded = dead_letters(&events)
        .find(|record| matches!(record.failure, SagaFailure::TellFailed { .. }))
        .expect("tell_failed dead letter must exist");
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
    type Codec = JsonCodec;
}

impl Aggregate for DrainTargetAgg {
    type Event = DrainDone;
    fn apply(&mut self, _event: DrainDone) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct DrainCmd;

impl Decider<DrainCmd> for DrainTargetAgg {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: DrainCmd) -> Decision<DrainDone, (), Self::Rejection> {
        Decision::persist(vec![DrainDone]).output(())
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
    aggregate_id: AggregateId,
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

    fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }
}

struct EndOnFirstSaga {
    target: BlockingDrainTellTarget,
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for EndOnFirstSaga {
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(END_ON_FIRST_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let Observed::Ping(event) = input;
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        // persist + one in-flight tell + end → enters Draining (stays alive to
        // receive and drop the second upstream event).
        let intent = TellIntent::new(self.target.clone(), DrainCmd);
        Ok(SagaEffect::persist(SagaLog { note: event.tag })
            .combine(SagaEffect::tell_intent(intent))
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
    use nitinol_saga::SagaFailure;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

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
    let saga_id = SagaId::new(END_ON_FIRST_SAGA_ID);
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
    let _proxy = SagaProps::<EndOnFirstSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| EndOnFirstSaga {
            target: BlockingDrainTellTarget {
                notify: Arc::clone(&drain_unblock_for_saga),
                aggregate_id: AggregateId::new("dlq-ended-target"),
            },
            handle_count: Arc::clone(&handle_count_for_saga),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(&upstream_store, "dlq-ended-upstream"))
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
        count_dead_letter_kind(&events, "ended_saga_received_message") >= 1,
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
    let decoded = dead_letters(&events)
        .find(|record| matches!(record.failure, SagaFailure::EndedSagaReceivedMessage { .. }))
        .expect("ended_saga_received_message dead letter must exist");
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
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_FAIL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let Observed::Ping(event) = input;
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
    let system = EventSourceSystem::builder(ps).build();

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
    let saga_id = SagaId::new(PERSIST_FAIL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let _proxy = SagaProps::<PersistFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistFailSaga {
            handled: Arc::clone(&handled_for_saga),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(
        &upstream_store,
        "dlq-persist-retry-ok-upstream",
    ))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Give the saga time to complete the retry and settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        count_dead_letter_kind(&events, "persist_failed"),
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
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-persist-upstream", 1, "persist-test").await;

    // Fails the first 3 appends (= default RetryPolicy::max_attempts).  The 4th
    // call (the DLQ enqueue) is routed to the inner store and succeeds, so the
    // dead letter is observable in the saga stream.
    let saga_store: Arc<dyn EventStore> = Arc::new(FailNAppendStore::new(3));
    let saga_id = SagaId::new(PERSIST_FAIL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let handled_for_saga = Arc::clone(&handled);
    let _proxy = SagaProps::<PersistFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistFailSaga {
            handled: Arc::clone(&handled_for_saga),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(
        &upstream_store,
        "dlq-persist-upstream",
    ))
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
        count_dead_letter_kind(&events, "persist_failed"),
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

/// A deadline message whose codec writes a payload happily and can never read
/// one back.  `encode` succeeds so the schedule fact is stored durably; the
/// decode failure is injected only at the timer-fire path, which is where the
/// persisted payload is read back through this very codec.
struct AlwaysDecodeFails;

#[derive(Debug)]
struct DecodeBoom;

impl std::fmt::Display for DecodeBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "intentional decode failure for DLQ test")
    }
}

impl std::error::Error for DecodeBoom {}

struct AlwaysDecodeFailsCodec;

impl Codec<AlwaysDecodeFails> for AlwaysDecodeFailsCodec {
    type Error = DecodeBoom;

    fn encode(_value: &AlwaysDecodeFails) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from_static(b"unreadable"))
    }

    fn decode(_payload: &[u8]) -> Result<AlwaysDecodeFails, Self::Error> {
        Err(DecodeBoom)
    }
}

impl DeadlineMessage for AlwaysDecodeFails {
    type Codec = AlwaysDecodeFailsCodec;
}

/// The alphabet of [`DecodeFailSaga`]: one observed source, plus a deadline
/// whose message can never be read back.
// The observed payload is decoded and delivered as the variant it belongs to;
// what this saga decides does not read it, which `dead_code` cannot tell apart
// from a field nothing fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum DecodeInput {
    Ping(Ping),
    #[subscribe(deadline)]
    Unreadable(AlwaysDecodeFails),
}

struct DecodeFailSaga {
    deadline: Deadline<AlwaysDecodeFails>,
}

#[async_trait]
impl Saga for DecodeFailSaga {
    type Subscribe = DecodeInput;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            DecodeInput::Ping(_) => Some(SagaId::new(DECODE_FAIL_SAGA_ID)),
            // A firing never travels the subscription path.
            DecodeInput::Unreadable(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        match input {
            // Set a deadline; the stored payload is what
            // `AlwaysDecodeFailsCodec::decode` unconditionally rejects.
            DecodeInput::Ping(_) => Ok(self.deadline.set(
                TimerName::new("decode-test"),
                Duration::from_millis(80),
                AlwaysDecodeFails,
            )),
            // Never reached — the read-back fails before the arm is entered.
            DecodeInput::Unreadable(_) => Ok(SagaEffect::None),
        }
    }
}

/// Given a saga whose deadline message can never be read back,
/// When the deadline it set fires,
/// Then a `decode_failed` dead letter is enqueued on the saga stream
/// immediately.
#[tokio::test]
async fn decode_failure_in_scheduled_payload_enqueues_a_decode_failed_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-decode-upstream", 1, "decode-test").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(DECODE_FAIL_SAGA_ID);

    let _proxy = SagaProps::<DecodeFailSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| DecodeFailSaga {
            deadline: components.deadline(),
        },
    )
    .subscribed_to::<Ping>(Subscription::stream(&upstream_store, "dlq-decode-upstream"))
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
        count_dead_letter_kind(&events, "decode_failed") >= 1,
        "a deadline payload that cannot be read back must enqueue a \
         `decode_failed` dead letter"
    );
}
