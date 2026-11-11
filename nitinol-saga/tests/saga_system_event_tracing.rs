//! Where a framework record lands on a stream, an operator can read which one.
//!
//! The `event_type` column names the declaration and nothing finer, so the arm
//! a framework record holds is no longer legible from the column alone. The
//! saga emits a structured tracing record at each point where it writes one, so
//! that arm-level visibility exists in the place operators already watch.
//!
//! Exactly three declarations can appear on a saga's own stream —
//! `nitinol.saga.schedule`, `nitinol.saga.consumed` and
//! `nitinol.saga.lifecycle` — and this file covers all three, because the
//! contract is about the *set* of records the framework writes itself: one of
//! them reporting and another not would leave an operator with a blind spot
//! they cannot see the shape of.
//!
//! ```gherkin
//!   Scenario: a framework record written, a record emitted
//!     Given a saga that registers a deadline, fires it, consumes an upstream
//!           input and terminates
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
//! The one exception is the consumed fact, whose context fields *are* the
//! progress an operator reads: see
//! [`the_position_a_batch_consumed_is_reported_with_its_own_arm`].
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
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_system_event_tracing`.

#[path = "common/helpers.rs"]
mod common;
use common::{JsonCodec, CONSUMED_MARKER, LIFECYCLE_MARKER, SCHEDULE_MARKER};

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

// The capture

/// The three fields a record about a framework record has to carry.
const EVENT_TYPE: &str = "event_type";
const ARM: &str = "arm";
const STREAM_ID: &str = "stream_id";

/// The context a record about a consumed fact adds to those three: which
/// upstream stream was read, which declared source of it, and how far.
const SOURCE: &str = "source";
const SOURCE_EVENT_TYPE: &str = "source_event_type";
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

// The three paths a framework record is written down as.  Spelled as literals
// so a rename of a declaration has to be made here too: the string is what an
// operator's dashboards and an existing journal both hold.

const SCHEDULE: &str = "nitinol.saga.schedule";
const CONSUMED: &str = "nitinol.saga.consumed";
const LIFECYCLE: &str = "nitinol.saga.lifecycle";

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

/// The alphabet of every saga below but the one that owns a deadline: one
/// observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Ping(Ping),
}

async fn append_ping(store: &Journal, stream_key: &str, sequence: u64, tag: &str) {
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

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
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
    store: &Journal,
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

// Scenario 1 — the scheduler's own records

const SCHEDULE_SAGA_ID: &str = "tracing-schedule-saga";

/// What the deadline below carries.  It carries nothing — the records the test
/// reads are about the registration and the firing, not about a payload — so
/// the format it names writes no bytes.
#[derive(Clone, Debug)]
struct Reminder;

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

/// The alphabet of [`ScheduleSaga`]: the observed source, plus the saga's own
/// time.
#[derive(SagaSubscribe)]
enum TimerInput {
    Ping(Ping),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

struct ScheduleSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<Notify>,
}

#[async_trait]
impl Saga for ScheduleSaga {
    type Subscribe = TimerInput;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            TimerInput::Ping(_) => Some(SagaId::new(SCHEDULE_SAGA_ID)),
            // A deadline firing is addressed to the instance that set it and
            // never travels the subscription path.
            TimerInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        match input {
            TimerInput::Ping(_event) => Ok(self.deadline.set(
                TimerName::new("reminder"),
                Duration::from_millis(80),
                Reminder,
            )),
            TimerInput::Reminder(Reminder) => {
                self.fired.notify_one();
                Ok(SagaEffect::None)
            }
        }
    }
}

/// Given a saga that sets a deadline, When the registration and the firing are
/// each written to its stream, Then both are reported under the schedule event
/// type with the arm each recorded.
///
/// Two arms share one declaration here, which is the whole reason the arm is
/// reported at all: `nitinol.saga.schedule` in the column cannot tell "a timer
/// was armed" from "a timer elapsed".
#[tokio::test]
async fn a_registered_and_a_fired_timer_are_reported_with_distinct_arms() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    // One journal: the upstream stream the saga observes and the saga's own
    // stream are tenants of the store the system distributes.
    let journal = system.persistence().event_store();
    append_ping(&journal, "tracing-schedule-upstream", 1, "tick").await;

    let saga_id = SagaId::new(SCHEDULE_SAGA_ID);
    let fired = Arc::new(Notify::new());
    let fired_for_saga = Arc::clone(&fired);

    let _proxy = SagaManagerProps::<ScheduleSaga>::new(move |components| ScheduleSaga {
        deadline: components.deadline(),
        fired: Arc::clone(&fired_for_saga),
    })
    .observing::<Ping>(Subscription::stream("tracing-schedule-upstream"))
    .spawn(&system)
    .await;

    // Two schedule rows: the registration and the firing.
    wait_for_rows(&journal, &saga_id, SCHEDULE_MARKER, 2).await;

    capture
        .wait_for(SCHEDULE_SAGA_ID, SCHEDULE, "scheduled")
        .await;
    capture.wait_for(SCHEDULE_SAGA_ID, SCHEDULE, "fired").await;
}

// Scenario 2 — the position a batch consumed is reported like any other
// framework record

const POSITION_SAGA_ID: &str = "tracing-position-saga";
const POSITION_UPSTREAM_KEY: &str = "tracing-position-upstream";

struct PositionSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PositionSaga {
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(POSITION_SAGA_ID))
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

/// Given a saga whose decision batch also carries the position of the input it
/// was decided from, When that batch lands, Then a record names the position's
/// event type, its arm and the stream — the framework reports every record it
/// writes itself, and this is one of them — and names the upstream stream, the
/// declared source and the sequence the write recorded, which is the progress
/// the arm alone leaves unsaid.
///
/// The consumed fact is the one framework record whose context fields are part
/// of the contract: `consumed` holds for every position ever written, so an
/// operator asking how far a saga has got reads nothing from the arm. The
/// declared source is named alongside the stream key because two declared
/// sources can share one key, and a report that named only the key would not
/// say which of them the position settled.
#[tokio::test]
async fn the_position_a_batch_consumed_is_reported_with_its_own_arm() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let journal = system.persistence().event_store();
    append_ping(&journal, POSITION_UPSTREAM_KEY, 1, "note").await;

    let saga_id = SagaId::new(POSITION_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handled_for_saga = Arc::clone(&handled);

    let _proxy = SagaManagerProps::<PositionSaga>::new(move |_components| PositionSaga {
        handled: Arc::clone(&handled_for_saga),
    })
    .observing::<Ping>(Subscription::stream(POSITION_UPSTREAM_KEY))
    .spawn(&system)
    .await;

    // The row has to be on the stream before the record about it means
    // anything: a record claiming a write that never happened would be worse
    // than none.
    wait_for_rows(&journal, &saga_id, CONSUMED_MARKER, 1).await;

    capture
        .wait_for(POSITION_SAGA_ID, CONSUMED, "consumed")
        .await;

    let reported = capture.records_of(POSITION_SAGA_ID, CONSUMED, "consumed");
    let progress: Vec<(Option<&String>, Option<&String>, Option<&String>)> = reported
        .iter()
        .map(|fields| {
            (
                fields.get(SOURCE),
                fields.get(SOURCE_EVENT_TYPE),
                fields.get(UPSTREAM_SEQUENCE),
            )
        })
        .collect();
    assert_eq!(
        progress,
        vec![(
            Some(&POSITION_UPSTREAM_KEY.to_owned()),
            Some(&Ping::EVENT_TYPE.to_string()),
            Some(&"1".to_owned())
        )],
        "the arm says a position landed but not which input it closed out; an \
         operator reading how far the saga has consumed each declared source of \
         each upstream stream needs the source, its event type and the sequence \
         named too; the capture held {reported:?}"
    );
}

// Scenario 3 — the saga's own termination

const LIFECYCLE_SAGA_ID: &str = "tracing-lifecycle-saga";

struct EndingSaga;

#[async_trait]
impl Saga for EndingSaga {
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(LIFECYCLE_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        let Observed::Ping(event) = input;
        Ok(SagaEffect::persist(SagaLog { note: event.tag }).then_end())
    }
}

/// Given a saga that terminates, When the `Ended` fact lands on its own stream,
/// Then a record names the lifecycle event type, the `ended` arm and the stream.
///
/// The termination is the last thing a saga ever writes, so it is also the last
/// chance to say so: nothing afterwards will report on this instance again, and
/// an unreported `Ended` leaves an operator watching a stream that has simply
/// gone quiet with no way to tell that from a stall.
#[tokio::test]
async fn a_terminated_saga_reports_the_ended_lifecycle_fact() {
    let capture = Capture::installed();
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let journal = system.persistence().event_store();
    append_ping(&journal, "tracing-lifecycle-upstream", 1, "last").await;

    let saga_id = SagaId::new(LIFECYCLE_SAGA_ID);

    let _proxy = SagaManagerProps::<EndingSaga>::new(|_components| EndingSaga)
        .observing::<Ping>(Subscription::stream("tracing-lifecycle-upstream"))
        .spawn(&system)
        .await;

    // The record is about a write, so the write is what the test waits on first.
    wait_for_rows(&journal, &saga_id, LIFECYCLE_MARKER, 1).await;

    capture
        .wait_for(LIFECYCLE_SAGA_ID, LIFECYCLE, "ended")
        .await;
}

// Scenario 4 — a user's own event is not reported

const DOMAIN_ONLY_SAGA_ID: &str = "tracing-domain-saga";

struct DomainOnlySaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for DomainOnlySaga {
    type Subscribe = Observed;
    type Event = SagaLog;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(DOMAIN_ONLY_SAGA_ID))
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let journal = system.persistence().event_store();
    append_ping(&journal, "tracing-domain-upstream", 1, "note").await;

    let saga_id = SagaId::new(DOMAIN_ONLY_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handled_for_saga = Arc::clone(&handled);

    let _proxy = SagaManagerProps::<DomainOnlySaga>::new(move |_components| DomainOnlySaga {
        handled: Arc::clone(&handled_for_saga),
    })
    .observing::<Ping>(Subscription::stream("tracing-domain-upstream"))
    .spawn(&system)
    .await;

    // The domain row has to be on the stream before its absence from the
    // capture means anything.
    wait_for_rows(&journal, &saga_id, SagaLog::EVENT_TYPE, 1).await;

    let named = capture.event_types(DOMAIN_ONLY_SAGA_ID);
    let user_path = SagaLog::EVENT_TYPE.to_path().to_string();
    assert!(
        !named.contains(&user_path),
        "a user's own event must not be reported as a framework record, and the capture named \
         {named:?} on this stream"
    );
}
