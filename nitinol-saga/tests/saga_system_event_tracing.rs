//! Where a framework record lands on a stream, an operator can read which one.
//!
//! The `event_type` column names the declaration and nothing finer, so the arm
//! a framework record holds is no longer legible from the column alone. The
//! saga emits a structured tracing record at each point where it writes one, so
//! that arm-level visibility exists in the place operators already watch.
//!
//! ```gherkin
//!   Scenario: a framework record written, a record emitted
//!     Given a saga that fails a handle, fails a tell, fires a timer and
//!           consumes an upstream input
//!     When each framework record lands on the saga's own stream
//!     Then a tracing record names the event type, the arm and the stream
//!
//!   Scenario: a user's own event
//!     Given a saga that only persists a domain event
//!     Then no such record is emitted for it — arm-level measurement of a
//!          user's events is the user's to add
//! ```
//!
//! ## What is pinned, and what is not
//!
//! Pinned: that a record carrying `event_type`, `arm` and `stream_id` is
//! emitted where each framework record is written, and the values of those three
//! fields. The level a record is emitted at, its target, its message and any
//! further context fields are not pinned here — a record is selected by carrying
//! the three required fields, so this file does not turn a level or a target
//! string into a contract the requirement never stated.
//!
//! Emission is tied to the *write*: every assertion below waits on the record
//! landing in the store first, then reads the capture. A tracing record claiming
//! a write that never happened would be worse than none.
//!
//! The capture is a `tracing::Subscriber` written out here rather than a
//! dependency: this crate already depends on `tracing`, and the work being
//! instrumented runs on runtime-owned threads, so a thread-scoped subscriber
//! would not see it. A process has one global subscriber, so the tests in this
//! binary share one and tell their records apart by `stream_id` — each test
//! drives a saga id of its own.

#[path = "common/helpers.rs"]
mod common;
use common::{JsonCodec, CONSUMED_POSITION_MARKER, DEAD_LETTER_MARKER, SCHEDULE_MARKER};

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};

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
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TimerName};

// The capture

/// The three fields a record about a framework record has to carry.
const EVENT_TYPE: &str = "event_type";
const ARM: &str = "arm";
const STREAM_ID: &str = "stream_id";

/// The context a record about a consumed position adds to those three: which
/// upstream stream was read, and how far.
const SOURCE: &str = "source";
const UPSTREAM_SEQUENCE: &str = "upstream_sequence";

/// Why the capture buffer's lock cannot be poisoned: it guards a push and a
/// read of a `Vec`, and no holder can panic.
const CAPTURE_LOCK: &str = "the capture buffer guards only a push and a read, so no holder panics";

/// One captured record, as its fields.
type Fields = BTreeMap<String, String>;

#[derive(Default)]
struct FieldMap(Fields);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // A `&str` recorded through `?` or `%` arrives here rendered with its
        // surrounding quotes; the value is the same either way, and the field's
        // value is what the requirement names.
        let rendered = format!("{value:?}");
        let unquoted = rendered
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(&rendered)
            .to_owned();
        self.0.insert(field.name().to_owned(), unquoted);
    }
}

struct Recorder {
    captured: Arc<Mutex<Vec<Fields>>>,
}

impl Subscriber for Recorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = FieldMap::default();
        event.record(&mut fields);
        self.captured.lock().expect(CAPTURE_LOCK).push(fields.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Fields>>>);

static CAPTURE: OnceLock<Capture> = OnceLock::new();

impl Capture {
    /// The one process-wide capture. A process has a single global subscriber,
    /// so every test in this binary reads the same buffer and selects its own
    /// records by `stream_id`.
    fn installed() -> Self {
        CAPTURE
            .get_or_init(|| {
                let captured = Arc::new(Mutex::new(Vec::new()));
                tracing::subscriber::set_global_default(Recorder {
                    captured: Arc::clone(&captured),
                })
                .expect("this binary installs the capture once, before anything else emits");
                Capture(captured)
            })
            .clone()
    }

    /// Records about a framework record written on `stream_id`, as
    /// `(event_type, arm)` pairs in the order they were emitted.
    ///
    /// A record qualifies by carrying all three required fields, so the
    /// selection does not rest on a level, a target or a message.
    fn system_records(&self, stream_id: &str) -> Vec<(String, String)> {
        self.0
            .lock()
            .expect(CAPTURE_LOCK)
            .iter()
            .filter_map(|fields| {
                let event_type = fields.get(EVENT_TYPE)?;
                let arm = fields.get(ARM)?;
                let stream = fields.get(STREAM_ID)?;
                (stream == stream_id).then(|| (event_type.clone(), arm.clone()))
            })
            .collect()
    }

    /// Every field of every record about `stream_id` that named `event_type`
    /// and `arm`.
    ///
    /// [`Self::system_records`] keeps only the three required fields; a
    /// record's own context fields are what say *which* write it was about.
    fn records_of(&self, stream_id: &str, event_type: &str, arm: &str) -> Vec<Fields> {
        self.0
            .lock()
            .expect(CAPTURE_LOCK)
            .iter()
            .filter(|fields| {
                fields.get(STREAM_ID).map(String::as_str) == Some(stream_id)
                    && fields.get(EVENT_TYPE).map(String::as_str) == Some(event_type)
                    && fields.get(ARM).map(String::as_str) == Some(arm)
            })
            .cloned()
            .collect()
    }

    /// Every `event_type` a record about `stream_id` named.
    fn event_types(&self, stream_id: &str) -> Vec<String> {
        self.system_records(stream_id)
            .into_iter()
            .map(|(event_type, _)| event_type)
            .collect()
    }

    /// Wait until a record naming `event_type` and `arm` on `stream_id` has been
    /// emitted, or report what was captured instead.
    async fn wait_for(&self, stream_id: &str, event_type: &str, arm: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let seen = self.system_records(stream_id);
            if seen.iter().any(|(t, a)| t == event_type && a == arm) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "no record named `{event_type}` arm `{arm}` on stream {stream_id:?}; \
                     the capture held {seen:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

// The paths a framework record is written down as

const OUTBOX: &str = "nitinol.saga.outbox";
const SCHEDULE: &str = "nitinol.saga.schedule";
const DEAD_LETTER: &str = "nitinol.saga.dead_letter";
const CONSUMED_POSITION: &str = "nitinol.saga.consumed_position";

// Domain fixtures

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    tag: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(Family::new("tracing_it"), TypeName::new("Ping"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog {
    note: String,
}

impl Event for SagaLog {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("tracing_it"), TypeName::new("SagaLog"));
    type Codec = JsonCodec;
}

#[derive(Debug)]
struct SagaBoom;

impl std::fmt::Display for SagaBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("saga boom")
    }
}

impl std::error::Error for SagaBoom {}

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

/// Wait until the saga's own stream holds at least `expected` rows of
/// `event_type` — the write a record is supposed to be about.
async fn wait_for_rows(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    event_type: EventType,
    expected: usize,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.iter().filter(|e| e.event_type == event_type).count() >= expected {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} `{event_type}` rows; the stream held {:?}",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Scenario 1 — a dead letter written, a record naming which failure it was

const HANDLE_FAIL_SAGA_ID: &str = "tracing-handle-saga";

struct HandleFailSaga;

#[async_trait]
impl Saga for HandleFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(HANDLE_FAIL_SAGA_ID))
    }
    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Err(SagaBoom)
    }
}

/// Given a saga whose `handle` fails, When the dead letter lands on its own
/// stream, Then a record names the dead-letter event type, the `handle_failed`
/// arm, and the stream it was written to.
#[tokio::test]
async fn a_dead_letter_is_reported_with_its_event_type_arm_and_stream() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "tracing-handle-upstream", 1, "boom").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(HANDLE_FAIL_SAGA_ID);

    let _proxy = SagaProps::<HandleFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || {
        HandleFailSaga
    })
    .with_subscription(
        Arc::clone(&upstream_store),
        SequenceCursor::Stream {
            key: "tracing-handle-upstream".to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    // The record is about a write, so the write is what the test waits on first.
    wait_for_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;

    capture
        .wait_for(HANDLE_FAIL_SAGA_ID, DEAD_LETTER, "handle_failed")
        .await;
}

// Scenario 2 — a tell that exhausts its budget, reported at both records it writes

const TELL_FAIL_SAGA_ID: &str = "tracing-tell-saga";
const TELL_FAIL_TARGET_ID: &str = "tracing-inventory-target";

struct FailingTellTarget<A: Aggregate> {
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
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("tracing_it"),
        TypeName::new("InventoryReserved"),
    );
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

struct TellFailSaga {
    target: FailingTellTarget<Inventory>,
}

#[async_trait]
impl Saga for TellFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(TELL_FAIL_SAGA_ID))
    }
    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Ok(SagaEffect::tell(self.target.clone(), Reserve))
    }
}

/// Given a tell that fails every attempt, When the outbox request, the terminal
/// outbox marker and the dead letter are written, Then each is reported with the
/// arm it recorded.
///
/// The three arms are what the column no longer distinguishes: `tell_requested`
/// and `tell_failed` share the outbox event type, and the dead letter's
/// `tell_failed` shares its event type with every other failure kind.
#[tokio::test]
async fn each_marker_of_a_failing_tell_is_reported_with_its_own_arm() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "tracing-tell-upstream", 1, "order").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TELL_FAIL_SAGA_ID);

    let _proxy =
        SagaProps::<TellFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || TellFailSaga {
            target: FailingTellTarget {
                target_id: AggregateId::new(TELL_FAIL_TARGET_ID),
                _phantom: PhantomData,
            },
        })
        .with_subscription(
            Arc::clone(&upstream_store),
            SequenceCursor::Stream {
                key: "tracing-tell-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    wait_for_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;

    capture
        .wait_for(TELL_FAIL_SAGA_ID, OUTBOX, "tell_requested")
        .await;
    capture
        .wait_for(TELL_FAIL_SAGA_ID, OUTBOX, "tell_failed")
        .await;
    capture
        .wait_for(TELL_FAIL_SAGA_ID, DEAD_LETTER, "tell_failed")
        .await;
}

// Scenario 3 — the scheduler's own records

const SCHEDULE_SAGA_ID: &str = "tracing-schedule-saga";

struct ScheduleSaga {
    fired: Arc<Notify>,
}

#[async_trait]
impl Saga for ScheduleSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(SCHEDULE_SAGA_ID))
    }
    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Ok(SagaEffect::schedule(
            TimerName::new("reminder"),
            Duration::from_millis(80),
            (),
        ))
    }

    async fn on_scheduled(
        &mut self,
        _message: (),
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.fired.notify_one();
        Ok(SagaEffect::None)
    }
}

/// Given a saga that registers a timer, When the registration and the firing are
/// each written to its stream, Then both are reported under the schedule event
/// type with the arm each recorded.
#[tokio::test]
async fn a_registered_and_a_fired_timer_are_reported_with_distinct_arms() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "tracing-schedule-upstream", 1, "tick").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SCHEDULE_SAGA_ID);
    let fired = Arc::new(Notify::new());
    let fired_for_saga = Arc::clone(&fired);

    let _proxy =
        SagaProps::<ScheduleSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            ScheduleSaga {
                fired: Arc::clone(&fired_for_saga),
            }
        })
        .with_subscription(
            Arc::clone(&upstream_store),
            SequenceCursor::Stream {
                key: "tracing-schedule-upstream".to_owned(),
                after: 0,
            },
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    // Two schedule rows: the registration and the firing.
    wait_for_rows(&saga_store, &saga_id, SCHEDULE_MARKER, 2).await;

    capture
        .wait_for(SCHEDULE_SAGA_ID, SCHEDULE, "scheduled")
        .await;
    capture.wait_for(SCHEDULE_SAGA_ID, SCHEDULE, "fired").await;
}

// Scenario 4 — the position a batch consumed is reported like any other
// framework record

const POSITION_SAGA_ID: &str = "tracing-position-saga";
const POSITION_UPSTREAM_KEY: &str = "tracing-position-upstream";

struct PositionSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PositionSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(POSITION_SAGA_ID))
    }
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

/// Given a saga whose decision batch also carries the position of the input it
/// was decided from, When that batch lands, Then a record names the position's
/// event type, its arm and the stream — the framework reports every record it
/// writes itself, and this is one of them — and names the upstream stream and
/// sequence the write recorded, which is the progress the arm alone leaves
/// unsaid.
#[tokio::test]
async fn the_position_a_batch_consumed_is_reported_with_its_own_arm() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, POSITION_UPSTREAM_KEY, 1, "note").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(POSITION_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handled_for_saga = Arc::clone(&handled);

    let _proxy =
        SagaProps::<PositionSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            PositionSaga {
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_subscription(
            Arc::clone(&upstream_store),
            SequenceCursor::Stream {
                key: POSITION_UPSTREAM_KEY.to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // The row has to be on the stream before the record about it means
    // anything: a record claiming a write that never happened would be worse
    // than none.
    wait_for_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    capture
        .wait_for(POSITION_SAGA_ID, CONSUMED_POSITION, "consumed")
        .await;

    let reported = capture.records_of(POSITION_SAGA_ID, CONSUMED_POSITION, "consumed");
    let progress: Vec<(Option<&String>, Option<&String>)> = reported
        .iter()
        .map(|fields| (fields.get(SOURCE), fields.get(UPSTREAM_SEQUENCE)))
        .collect();
    assert_eq!(
        progress,
        vec![(
            Some(&POSITION_UPSTREAM_KEY.to_owned()),
            Some(&"1".to_owned())
        )],
        "the arm says a position landed but not which input it closed out; an \
         operator reading how far the saga has consumed each upstream stream \
         needs the source and the sequence named too; the capture held {reported:?}"
    );
}

// Scenario 5 — a user's own event is not reported

const DOMAIN_ONLY_SAGA_ID: &str = "tracing-domain-saga";

struct DomainOnlySaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for DomainOnlySaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = std::convert::Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(DOMAIN_ONLY_SAGA_ID))
    }
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

/// Given a saga that only persists an event of its own, When that event is
/// written, Then no record about a framework record names it.
///
/// Arm-level measurement of a user's events is the user's to add: the framework
/// reports what it wrote itself and nothing further, so a deployment's records
/// do not grow with every domain event a saga persists.
#[tokio::test]
async fn a_user_event_is_not_reported_as_a_framework_record() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "tracing-domain-upstream", 1, "note").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(DOMAIN_ONLY_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handled_for_saga = Arc::clone(&handled);

    let _proxy =
        SagaProps::<DomainOnlySaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            DomainOnlySaga {
                handled: Arc::clone(&handled_for_saga),
            }
        })
        .with_subscription(
            Arc::clone(&upstream_store),
            SequenceCursor::Stream {
                key: "tracing-domain-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // The domain row has to be on the stream before its absence from the
    // capture means anything.
    wait_for_rows(&saga_store, &saga_id, SagaLog::EVENT_TYPE, 1).await;

    let named = capture.event_types(DOMAIN_ONLY_SAGA_ID);
    let user_path = SagaLog::EVENT_TYPE.to_path().to_string();
    assert!(
        !named.contains(&user_path),
        "a user's own event must not be reported as a framework record, and the capture named \
         {named:?} on this stream"
    );
}
