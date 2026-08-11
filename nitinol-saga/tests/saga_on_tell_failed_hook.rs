//! `Saga::on_tell_failed`: a tell that exhausts its staged retry budget must be
//! reported to the saga at the moment the outbox executor settles — not when
//! the next upstream event happens to arrive — and the effect the hook returns
//! must be interpreted exactly like the one `handle` returns.
//!
//! Scenarios pinned here:
//!
//! - Running lifecycle with a known intent: the hook runs once, receives the
//!   target aggregate's stream key, and its compensating effect reaches the
//!   saga's own stream while no further upstream event is ever published.
//! - The failure bookkeeping that already surrounded the outcome report stays
//!   in place: the terminal `tell_failed` outbox marker and the `tell_failed`
//!   dead letter still land on the saga stream.
//! - Draining lifecycle: the report is dropped without reaching the hook, and
//!   the failure survives as a `tell_failed` dead letter.
//! - A failure whose dead letter cannot be written does not reach the hook
//!   either: compensation must not be driven by an unrecorded failure.
//! - Failures surfaced by replay keep flowing through
//!   `SagaContext::failed_tell_ids`; the hook is not involved.
//! - A failure the hook consumed is withheld from **both** readers of
//!   `SagaContext::failed_tell_ids` — `on_scheduled` as well as `handle`.
//! - A hook returning `Err` is recorded as a `tell_failed_hook_failed` dead
//!   letter rather than logged and discarded.
//!
//! Every wait loop polls the saga's own EventStore only.  Nothing in this file
//! publishes a second upstream event after the one that starts the saga, so a
//! compensation observed here can only have come from the tell-failure report.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_outbox_tell_failed, encode_outbox_tell_requested, outbox_kind_of, JsonCodec, OutboxKind,
    OUTBOX_MARKER,
};

use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

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
use nitinol_saga::{
    spawn_scheduler, Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent, TimerName,
};

/// Stream key reported by the always-failing tell target.  The hook must
/// receive exactly this value as its `target` argument.
const TELL_TARGET_ID: &str = "on-tell-failed-inventory";

/// Note written by `handle` when it dispatches the doomed tell.
const STARTED_NOTE: &str = "started";

/// Note written by the compensating effect the hook returns.  Its presence on
/// the saga stream is the observable proof that the hook's effect was
/// interpreted.
const COMPENSATION_NOTE: &str = "compensated";

/// Timer the hook registers so that a second reader of
/// `SagaContext::failed_tell_ids` runs after the failure was consumed.
const RECHECK_TIMER: &str = "on-tell-failed-recheck";

const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("on_tell_failed"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SagaLog {
    note: String,
}

impl Event for SagaLog {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("on_tell_failed"), TypeName::new("SagaLog"));
}

// Tell target that fails on every attempt, so the executor always exhausts its
// staged retry budget and reports `TellOutcome::Failed`.

#[derive(Default)]
struct Inventory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved;

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("on_tell_failed"),
        TypeName::new("InventoryReserved"),
    );
}

impl Aggregate for Inventory {
    type Event = InventoryReserved;
    fn apply(&mut self, _event: InventoryReserved) {}
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Reserve;

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = Infallible;

    async fn decide(
        &self,
        _cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<InventoryReserved>, Self::Rejection> {
        Ok(Effect::persist(InventoryReserved))
    }
}

struct FailingTellTarget<A: Aggregate> {
    /// Reported as `aggregate_id_str()`, so `TellIntent` captures it as the
    /// intent's target and the framework can hand it to the failure hook.
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

impl<A: Aggregate> FailingTellTarget<A> {
    fn new(target_id: &'static str) -> Self {
        Self {
            target_id,
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

// Recorder shared between the test and the saga under test.

/// One `on_tell_failed` invocation as the saga saw it.
#[derive(Clone, Debug, PartialEq)]
struct HookCall {
    /// The `target` argument, stringified.
    target: String,
    /// `ctx.failed_tell_ids()` as observed inside the hook.
    failed_tell_ids: Vec<u64>,
}

#[derive(Default)]
struct Recorder {
    hook_calls: Mutex<Vec<HookCall>>,
    handle_calls: Mutex<Vec<Vec<u64>>>,
    scheduled_calls: Mutex<Vec<Vec<u64>>>,
}

const POISON: &str = "recorder mutex is never poisoned: no holder panics while the guard is alive";

impl Recorder {
    fn record_hook(&self, target: &SagaId, ctx: &SagaContext) {
        self.hook_calls.lock().expect(POISON).push(HookCall {
            target: target.as_str().to_owned(),
            failed_tell_ids: ctx.failed_tell_ids().to_vec(),
        });
    }

    fn record_handle(&self, ctx: &SagaContext) {
        self.handle_calls
            .lock()
            .expect(POISON)
            .push(ctx.failed_tell_ids().to_vec());
    }

    fn record_scheduled(&self, ctx: &SagaContext) {
        self.scheduled_calls
            .lock()
            .expect(POISON)
            .push(ctx.failed_tell_ids().to_vec());
    }

    fn hook_calls(&self) -> Vec<HookCall> {
        self.hook_calls.lock().expect(POISON).clone()
    }

    fn handle_calls(&self) -> Vec<Vec<u64>> {
        self.handle_calls.lock().expect(POISON).clone()
    }

    fn scheduled_calls(&self) -> Vec<Vec<u64>> {
        self.scheduled_calls.lock().expect(POISON).clone()
    }
}

type SharedRecorder = Arc<Recorder>;

// Sagas under test

/// Dispatches one always-failing tell on its first upstream event and
/// compensates from `on_tell_failed`.
///
/// `terminate_after_tell` decides which lifecycle the outcome report meets:
/// `false` leaves the saga `Running`, `true` appends `End` so the saga is
/// draining its single in-flight tell when the failure is reported.
struct TellFailureSaga {
    target: FailingTellTarget<Inventory>,
    recorder: SharedRecorder,
    terminate_after_tell: bool,
}

#[async_trait]
impl Saga for TellFailureSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = SagaLog;
    type State = ();
    type ScheduledMessage = ();
    type Error = Infallible;

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_handle(ctx);

        let intent = TellIntent::new(self.target.clone(), Reserve);
        let effect = SagaEffect::persist(SagaLog {
            note: STARTED_NOTE.to_owned(),
        })
        .with_tells(vec![intent]);

        Ok(if self.terminate_after_tell {
            effect.then_end()
        } else {
            effect
        })
    }

    async fn on_tell_failed(
        &mut self,
        target: SagaId,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_hook(&target, ctx);
        Ok(SagaEffect::persist(SagaLog {
            note: COMPENSATION_NOTE.to_owned(),
        }))
    }
}

/// Emits no tell of its own — it exists to observe how a failure that entered
/// the process through replay is delivered.
struct ReplayObservingSaga {
    recorder: SharedRecorder,
}

#[async_trait]
impl Saga for ReplayObservingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = SagaLog;
    type State = ();
    type ScheduledMessage = ();
    type Error = Infallible;

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_handle(ctx);
        Ok(SagaEffect::None)
    }

    async fn on_tell_failed(
        &mut self,
        target: SagaId,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_hook(&target, ctx);
        Ok(SagaEffect::None)
    }
}

/// Payload of the timer the hook registers.  It carries nothing: the timer
/// exists only to reach `on_scheduled`, the second reader of
/// `SagaContext::failed_tell_ids`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recheck;

/// Dispatches one always-failing tell, then schedules a timer from inside
/// `on_tell_failed`.
///
/// Because the timer is registered by the hook itself, the `on_scheduled` call
/// it produces is necessarily later than the failure the hook consumed — which
/// is what makes the drain observable from that entry point without relying on
/// wall-clock ordering between two independent effects.
struct ScheduleAfterFailureSaga {
    target: FailingTellTarget<Inventory>,
    recorder: SharedRecorder,
}

#[async_trait]
impl Saga for ScheduleAfterFailureSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = SagaLog;
    type State = ();
    type ScheduledMessage = Recheck;
    type Error = Infallible;

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_handle(ctx);

        let intent = TellIntent::new(self.target.clone(), Reserve);
        Ok(SagaEffect::persist(SagaLog {
            note: STARTED_NOTE.to_owned(),
        })
        .with_tells(vec![intent]))
    }

    async fn on_tell_failed(
        &mut self,
        target: SagaId,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_hook(&target, ctx);
        Ok(SagaEffect::schedule(
            TimerName::new(RECHECK_TIMER),
            Duration::from_millis(50),
            Recheck,
        ))
    }

    async fn on_scheduled(
        &mut self,
        _message: Recheck,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.recorder.record_scheduled(ctx);
        Ok(SagaEffect::None)
    }
}

#[derive(Debug)]
struct HookRejected;

impl std::fmt::Display for HookRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("compensation hook refused to handle the tell failure")
    }
}

impl std::error::Error for HookRejected {}

/// Dispatches an always-failing tell and then rejects the failure report from
/// inside the hook.
struct RejectingHookSaga {
    target: FailingTellTarget<Inventory>,
}

#[async_trait]
impl Saga for RejectingHookSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = SagaLog;
    type State = ();
    type ScheduledMessage = ();
    type Error = HookRejected;

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: OrderPlaced,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let intent = TellIntent::new(self.target.clone(), Reserve);
        Ok(SagaEffect::persist(SagaLog {
            note: STARTED_NOTE.to_owned(),
        })
        .with_tells(vec![intent]))
    }

    async fn on_tell_failed(
        &mut self,
        _target: SagaId,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Err(HookRejected)
    }
}

// Store helpers

/// Accepts every append except the `tell_failed` dead letter, so a scenario can
/// reach the state where a tell failure carries a terminal outbox marker but no
/// durable dead-letter record.
#[derive(Default)]
struct RejectTellFailedDeadLetterStore {
    inner: InMemoryEventStore,
}

fn is_tell_failed_dead_letter(event: &AppendingEvent) -> bool {
    event.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
        && event.event_type.variant() == Some(Variant::new("tell_failed"))
}

#[async_trait]
impl EventStore for RejectTellFailedDeadLetterStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if events.iter().any(is_tell_failed_dead_letter) {
            return Err(AppendError::Backend(
                "injected: the tell_failed dead letter cannot be written".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
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

async fn append_order_placed(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64) {
    let payload = serde_json::to_vec(&OrderPlaced {
        order_id: format!("order-{sequence}"),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            stream_key,
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

fn count_dead_letters(events: &[LoadedEvent], variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
        .count()
}

fn count_terminal_tell_failed_markers(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count()
}

fn saga_log_notes(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event_type.type_key() == SagaLog::EVENT_TYPE.type_key())
        .map(|e| {
            serde_json::from_slice::<SagaLog>(&e.payload)
                .expect("a SagaLog payload must decode as raw codec output")
                .note
        })
        .collect()
}

fn event_types(events: &[LoadedEvent]) -> Vec<String> {
    events.iter().map(|e| e.event_type.to_string()).collect()
}

/// Poll the saga stream until `variant` appears as a dead letter, or until the
/// deadline passes.  Returns the last snapshot either way so the caller's
/// assertion — not this helper — reports what was missing.
async fn wait_for_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    variant: &'static str,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_dead_letters(&events, variant) >= 1 || Instant::now() >= deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Scenario runners

/// What a completed scenario left behind.
struct Observations {
    hook_calls: Vec<HookCall>,
    handle_calls: Vec<Vec<u64>>,
    scheduled_calls: Vec<Vec<u64>>,
    saga_events: Vec<LoadedEvent>,
}

/// Run one upstream event through [`TellFailureSaga`] and wait until the tell
/// has exhausted its retries and everything the report triggers has settled.
///
/// Exactly one upstream event is ever published, before the tell fails.  Any
/// compensation observed afterwards therefore cannot have been produced by a
/// later upstream delivery.
async fn run_tell_failure_scenario(saga_key: &str, terminate_after_tell: bool) -> Observations {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_key = format!("{saga_key}-upstream");
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_order_placed(&upstream_store, &upstream_key, 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(saga_key);
    let recorder: SharedRecorder = Arc::new(Recorder::default());

    let recorder_for_saga = Arc::clone(&recorder);
    let routed = saga_id.clone();
    let _saga_proxy =
        SagaProps::<TellFailureSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TellFailureSaga {
                target: FailingTellTarget::new(TELL_TARGET_ID),
                recorder: Arc::clone(&recorder_for_saga),
                terminate_after_tell,
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: upstream_key.clone(),
                after: 0,
            },
            move |_event: &OrderPlaced| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    // The terminal failure bookkeeping is unconditional, so it is the common
    // synchronisation point for both lifecycles.
    wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "tell_failed",
        Duration::from_secs(10),
    )
    .await;

    // Then leave a generous window in which a compensation would appear if the
    // hook ran.  The window elapses in full when no compensation is expected,
    // which is what makes the negative assertions meaningful.
    let deadline = Instant::now() + Duration::from_secs(3);
    let saga_events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let compensated = saga_log_notes(&events)
            .iter()
            .any(|note| note == COMPENSATION_NOTE);
        if compensated || Instant::now() >= deadline {
            break events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    if terminate_after_tell {
        assert_reported_while_draining(&saga_events);
    }

    Observations {
        hook_calls: recorder.hook_calls(),
        handle_calls: recorder.handle_calls(),
        scheduled_calls: recorder.scheduled_calls(),
        saga_events,
    }
}

/// The draining variant of the scenario only means anything if the saga had
/// already written its durable `Ended` marker when the tell failure settled.
/// Pinning that ordering keeps a draining test from passing because the saga
/// happened to be somewhere else in its lifecycle.
fn assert_reported_while_draining(events: &[LoadedEvent]) {
    let ended_at = events
        .iter()
        .position(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Ended(_))));
    let failed_at = events
        .iter()
        .position(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))));

    let (Some(ended), Some(failed)) = (ended_at, failed_at) else {
        panic!(
            "the draining scenario needs both an `Ended` marker and a terminal \
             `tell_failed` marker on the saga stream; it held: {:?}",
            event_types(events)
        );
    };
    assert!(
        ended < failed,
        "the saga must already have ended when the tell failure settled, \
         otherwise the report never met a draining lifecycle; \
         saga stream held: {:?}",
        event_types(events)
    );
}

/// Run one upstream event through [`TellFailureSaga`] on a store that refuses
/// the `tell_failed` dead letter, and wait until the terminal outbox marker
/// shows the tell has settled.
async fn run_undurable_failure_scenario(saga_key: &str) -> Observations {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_key = format!("{saga_key}-upstream");
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_order_placed(&upstream_store, &upstream_key, 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(RejectTellFailedDeadLetterStore::default());
    let saga_id = SagaId::new(saga_key);
    let recorder: SharedRecorder = Arc::new(Recorder::default());

    let recorder_for_saga = Arc::clone(&recorder);
    let routed = saga_id.clone();
    let _saga_proxy =
        SagaProps::<TellFailureSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TellFailureSaga {
                target: FailingTellTarget::new(TELL_TARGET_ID),
                recorder: Arc::clone(&recorder_for_saga),
                terminate_after_tell: false,
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: upstream_key.clone(),
                after: 0,
            },
            move |_event: &OrderPlaced| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    // The terminal marker is written before the dead letter is attempted, so it
    // is the synchronisation point that proves the rejected append was reached.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        if count_terminal_tell_failed_markers(&events) >= 1 || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Window in which a compensation would appear if the hook wrongly ran.
    tokio::time::sleep(Duration::from_secs(1)).await;

    Observations {
        hook_calls: recorder.hook_calls(),
        handle_calls: recorder.handle_calls(),
        scheduled_calls: recorder.scheduled_calls(),
        saga_events: load_saga_events(&saga_store, &saga_id).await,
    }
}

/// Seed a saga stream that already carries a settled `TellFailed`, then run one
/// upstream event through it.  The failure enters this incarnation through
/// replay, never through an outcome report.
async fn run_replay_failure_scenario(saga_key: &str) -> Observations {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_key = format!("{saga_key}-upstream");
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(saga_key);

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

    let recorder: SharedRecorder = Arc::new(Recorder::default());
    let recorder_for_saga = Arc::clone(&recorder);
    let routed = saga_id.clone();
    let _saga_proxy = SagaProps::<ReplayObservingSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move || ReplayObservingSaga {
            recorder: Arc::clone(&recorder_for_saga),
        },
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: upstream_key.clone(),
            after: 0,
        },
        move |_event: &OrderPlaced| Some(routed.clone()),
    )
    .spawn(system.process_system())
    .await;

    // Let replay finish before the first upstream event arrives, so the
    // failure is already in the process when `handle` runs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    append_order_placed(&upstream_store, &upstream_key, 1).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while recorder.handle_calls().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Window in which a hook invocation would show up if replay wrongly routed
    // the failure to it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    Observations {
        hook_calls: recorder.hook_calls(),
        handle_calls: recorder.handle_calls(),
        scheduled_calls: recorder.scheduled_calls(),
        saga_events: load_saga_events(&saga_store, &saga_id).await,
    }
}

/// Run one upstream event through [`ScheduleAfterFailureSaga`] and wait until
/// the timer its hook registered has fired.
async fn run_timer_after_failure_scenario(saga_key: &str) -> Observations {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = spawn_scheduler(system.process_system()).await;

    let upstream_key = format!("{saga_key}-upstream");
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_order_placed(&upstream_store, &upstream_key, 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(saga_key);
    let recorder: SharedRecorder = Arc::new(Recorder::default());

    let recorder_for_saga = Arc::clone(&recorder);
    let routed = saga_id.clone();
    let _saga_proxy = SagaProps::<ScheduleAfterFailureSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move || ScheduleAfterFailureSaga {
            target: FailingTellTarget::new(TELL_TARGET_ID),
            recorder: Arc::clone(&recorder_for_saga),
        },
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: upstream_key.clone(),
            after: 0,
        },
        move |_event: &OrderPlaced| Some(routed.clone()),
    )
    .with_scheduler(scheduler)
    .spawn(system.process_system())
    .await;

    // Only one upstream event is ever published, so the timer can only exist
    // because the hook ran and its effect was interpreted.
    let deadline = Instant::now() + Duration::from_secs(10);
    while recorder.scheduled_calls().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Observations {
        hook_calls: recorder.hook_calls(),
        handle_calls: recorder.handle_calls(),
        scheduled_calls: recorder.scheduled_calls(),
        saga_events: load_saga_events(&saga_store, &saga_id).await,
    }
}

/// Run one upstream event through [`RejectingHookSaga`] and return the saga
/// stream once the rejected hook has been accounted for (or the wait expires).
async fn run_rejecting_hook_scenario(saga_key: &str) -> Vec<LoadedEvent> {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_key = format!("{saga_key}-upstream");
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_order_placed(&upstream_store, &upstream_key, 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(saga_key);

    let routed = saga_id.clone();
    let _saga_proxy =
        SagaProps::<RejectingHookSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            RejectingHookSaga {
                target: FailingTellTarget::new(TELL_TARGET_ID),
            }
        })
        .with_codec(system.codec::<SagaLog>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: upstream_key.clone(),
                after: 0,
            },
            move |_event: &OrderPlaced| Some(routed.clone()),
        )
        .spawn(system.process_system())
        .await;

    wait_for_dead_letter(
        &saga_store,
        &saga_id,
        "tell_failed_hook_failed",
        Duration::from_secs(10),
    )
    .await
}

// Running lifecycle

/// Given a saga whose only tell fails on every attempt,
/// When the staged retry budget is exhausted while the saga is still running,
/// Then `on_tell_failed` is invoked exactly once and receives the target
/// aggregate's stream key.
#[tokio::test]
async fn running_saga_is_notified_once_with_the_failing_tell_target() {
    let obs = run_tell_failure_scenario("on-tell-failed-running-target", false).await;

    assert_eq!(
        obs.hook_calls.len(),
        1,
        "the tell-failure hook must run exactly once for one failed tell; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert_eq!(
        obs.hook_calls[0].target, TELL_TARGET_ID,
        "the hook must receive the target aggregate's stream key, \
         not the saga's own id or an internal tell id"
    );
}

/// Given a saga that compensates from `on_tell_failed`,
/// When the tell fails and no further upstream event is ever published,
/// Then the compensating effect is interpreted and its event lands on the
/// saga's own stream.
#[tokio::test]
async fn compensating_effect_is_interpreted_without_any_further_upstream_event() {
    let obs = run_tell_failure_scenario("on-tell-failed-running-compensate", false).await;

    let notes = saga_log_notes(&obs.saga_events);
    assert_eq!(
        notes,
        vec![STARTED_NOTE.to_owned(), COMPENSATION_NOTE.to_owned()],
        "the saga stream must carry the compensation the hook returned, \
         appended after the event that dispatched the tell; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert_eq!(
        obs.handle_calls.len(),
        1,
        "`handle` must have run exactly once: the compensation is driven by the \
         tell-failure report alone, with no second upstream event involved"
    );
}

/// Given a saga notified through `on_tell_failed`,
/// When the hook runs,
/// Then the context it receives carries exactly the failed tell id, so a saga
/// with several outstanding tells can tell them apart.
#[tokio::test]
async fn hook_context_carries_exactly_the_failed_tell_id() {
    let obs = run_tell_failure_scenario("on-tell-failed-running-context", false).await;

    assert_eq!(
        obs.hook_calls.len(),
        1,
        "the tell-failure hook must run exactly once for one failed tell"
    );
    assert_eq!(
        obs.hook_calls[0].failed_tell_ids.len(),
        1,
        "the context handed to the hook must carry exactly the one tell id this \
         invocation is about; got {:?}",
        obs.hook_calls[0].failed_tell_ids
    );
}

/// Given the hook is now called on the outcome report,
/// When the report is processed,
/// Then the durable failure record that already existed is still written: the
/// terminal outbox marker and the `tell_failed` dead letter.
#[tokio::test]
async fn notifying_the_hook_keeps_the_terminal_marker_and_the_dead_letter() {
    let obs = run_tell_failure_scenario("on-tell-failed-running-durability", false).await;

    assert_eq!(
        count_terminal_tell_failed_markers(&obs.saga_events),
        1,
        "the terminal `tell_failed` outbox marker must still be appended; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert_eq!(
        count_dead_letters(&obs.saga_events, "tell_failed"),
        1,
        "the `tell_failed` dead letter must still be enqueued; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
}

// Draining lifecycle

/// Given a saga that ended while one tell was still in flight,
/// When that tell then exhausts its retries,
/// Then the report is dropped: the hook does not run and no compensating
/// effect reaches the stream.
#[tokio::test]
async fn draining_saga_drops_the_failure_instead_of_notifying_the_hook() {
    let obs = run_tell_failure_scenario("on-tell-failed-draining-drop", true).await;

    assert!(
        obs.hook_calls.is_empty(),
        "a draining saga must not be notified; the hook saw {:?}",
        obs.hook_calls
    );
    assert_eq!(
        saga_log_notes(&obs.saga_events),
        vec![STARTED_NOTE.to_owned()],
        "no compensating event may be appended after the saga has ended; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
}

/// Given a failure dropped because the saga was draining,
/// When the report has been processed,
/// Then the failure is still durable as a `tell_failed` dead letter.
#[tokio::test]
async fn draining_saga_still_records_the_dropped_failure_as_a_dead_letter() {
    let obs = run_tell_failure_scenario("on-tell-failed-draining-dlq", true).await;

    assert_eq!(
        count_dead_letters(&obs.saga_events, "tell_failed"),
        1,
        "dropping the report must not drop the failure: exactly one \
         `tell_failed` dead letter must remain; saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
}

// Failure that could not be recorded

/// Given a tell failure whose `tell_failed` dead letter cannot be written,
/// When the report is processed,
/// Then the hook is not notified and no compensation is interpreted: a
/// compensation must never be driven by a failure the saga could not record.
#[tokio::test]
async fn hook_is_not_notified_when_the_failure_could_not_be_recorded() {
    let obs = run_undurable_failure_scenario("on-tell-failed-undurable").await;

    assert_eq!(
        count_terminal_tell_failed_markers(&obs.saga_events),
        1,
        "the scenario only means anything once the tell has settled with its \
         terminal marker; saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert_eq!(
        count_dead_letters(&obs.saga_events, "tell_failed"),
        0,
        "the injected store must have kept the dead letter off the stream; \
         saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert!(
        obs.hook_calls.is_empty(),
        "the hook must not run for a failure that is not durably recorded; \
         the hook saw {:?}",
        obs.hook_calls
    );
    assert_eq!(
        saga_log_notes(&obs.saga_events),
        vec![STARTED_NOTE.to_owned()],
        "no compensating event may be appended; saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
}

// The other reader of `failed_tell_ids`

/// Given a tell failure already consumed by `on_tell_failed`,
/// When a timer registered by that hook fires afterwards,
/// Then `on_scheduled` — the second reader of `ctx.failed_tell_ids()` — sees an
/// empty slice, so the failure is withheld from every drain entry point rather
/// than from `handle` alone.
#[tokio::test]
async fn failure_consumed_by_the_hook_is_not_repeated_to_on_scheduled() {
    let obs = run_timer_after_failure_scenario("on-tell-failed-timer-drain").await;

    assert_eq!(
        obs.hook_calls.len(),
        1,
        "the scenario only means anything once the hook has consumed the \
         failure; saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert!(
        !obs.scheduled_calls.is_empty(),
        "the timer the hook scheduled must have fired, otherwise the second \
         drain entry point was never reached; saga stream held: {:?}",
        event_types(&obs.saga_events)
    );
    assert!(
        obs.scheduled_calls.iter().all(|ids| ids.is_empty()),
        "`on_scheduled` must not be told about a failure the hook already \
         consumed; it saw {:?}",
        obs.scheduled_calls
    );
}

// Replay-sourced failures

/// Given a saga whose stream already carried a settled `TellFailed` on start,
/// When the first upstream event arrives,
/// Then the failure reaches `handle` through `ctx.failed_tell_ids()` and the
/// hook is left out of it.
#[tokio::test]
async fn replayed_failure_reaches_handle_and_not_the_hook() {
    let obs = run_replay_failure_scenario("on-tell-failed-replay").await;

    assert_eq!(
        obs.handle_calls,
        vec![vec![1u64]],
        "the replayed tell id must be surfaced to the first `handle` call"
    );
    assert!(
        obs.hook_calls.is_empty(),
        "a failure recovered by replay must not be routed to the hook; \
         the hook saw {:?}",
        obs.hook_calls
    );
}

// Rejected hook

/// Given a hook that returns `Err`,
/// When the tell failure is reported to it,
/// Then the rejection is enqueued under its own dead-letter variant instead of
/// being logged and discarded, and no compensation is interpreted.
#[tokio::test]
async fn rejected_hook_is_recorded_as_its_own_dead_letter() {
    let events = run_rejecting_hook_scenario("on-tell-failed-hook-rejects").await;

    assert_eq!(
        count_dead_letters(&events, "tell_failed_hook_failed"),
        1,
        "a hook that returns Err must be enqueued as exactly one \
         `tell_failed_hook_failed` dead letter; saga stream held: {:?}",
        event_types(&events)
    );
    assert_eq!(
        saga_log_notes(&events),
        vec![STARTED_NOTE.to_owned()],
        "a rejected hook produces no effect, so no further saga event may be \
         appended; saga stream held: {:?}",
        event_types(&events)
    );
}
