//! Shared fixtures for the read-side deadline index. Each test binary compiles
//! this module in full but uses only the subset it needs, so per-binary dead
//! code is expected.
//!
//! A binary that includes this module also includes `common/helpers.rs` as
//! `common`: the schedule and lifecycle mirrors live there, and this module
//! writes its seeded facts through them.
//!
//! The saga below declares a deadline, so a host over it starts the index. The
//! facts a test seeds are written straight onto a saga's own stream, the state
//! a crash or a stopped system leaves behind, so the journal is the whole of
//! what the index has to go on.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, SagaOffsetSlot, Unprovided};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryOffsetStore, InMemoryStore, Journal,
};
use nitinol_persistence::{
    AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaHostProxy, SagaId,
    SagaManagerProps, SagaSubscribe, Subscription, TimerName,
};

use crate::common::{
    encode_lifecycle_ended, encode_schedule_scheduled, schedule_kind_of, schedule_transition,
    CancelledMarkerPayload, FiredMarkerPayload, JsonCodec, ScheduleKind, ScheduleMarkerPayload,
    LIFECYCLE_MARKER, SCHEDULE_MARKER,
};

/// Poll interval every host here is wired with. It is the delivery host's
/// cadence, and the index shares it, so one setting makes both quick.
pub const TEST_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a probe waits for something the index or the instance it wakes
/// does on its own loop.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a probe re-reads what it is waiting for.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(25);

/// Why a fixture lock cannot be poisoned: every holder does a read or a push
/// and returns.
pub const FIXTURE_LOCK: &str = "a fixture lock guards only a read or a push, so no holder panics";

/// The one timer name the saga below sets from a [`Trigger`].
pub const REMINDER: &str = "reminder";

// Domain

/// The upstream input that makes an instance set its deadline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trigger {
    /// The instance this trigger is correlated to.
    pub saga: String,
    pub note: String,
    pub after_millis: u64,
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_index_it"), TypeName::new("Trigger"));
    type Codec = JsonCodec;
}

/// What the deadline arm writes down when a firing reaches it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recorded {
    pub label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_index_it"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// The message the saga promises its future self.
#[derive(Clone, Debug, PartialEq)]
pub struct Reminder {
    pub note: String,
}

/// The bytes of a [`Reminder`] are its note, so a seeded schedule fact and one
/// the saga wrote itself read back alike.
pub struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.note.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder {
            note: String::from_utf8_lossy(payload).into_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "deadline-index-reminder";
    const CODEC_NAME: &'static str = "deadline-index-reminder-codec";
    type Codec = ReminderCodec;
}

/// The message type a schedule fact written by some other saga type names.
///
/// Every saga type's schedule facts share one event type, so this is what a
/// row the index must leave alone looks like.
pub const FOREIGN_MESSAGE_TYPE: &str = "deadline-index-someone-elses-reminder";

#[derive(SagaSubscribe)]
pub enum IndexInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// The notes every firing that reached `handle` carried, in arrival order.
pub type Fired = Arc<Mutex<Vec<String>>>;

pub fn fired() -> Fired {
    Arc::new(Mutex::new(Vec::new()))
}

/// A saga whose alphabet declares a deadline: a trigger sets one, and the
/// deadline arm records the note it carried.
pub struct IndexSaga {
    deadline: Deadline<Reminder>,
    fired: Fired,
}

#[async_trait]
impl Saga for IndexSaga {
    type Subscribe = IndexInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            IndexInput::Trigger(trigger) => Some(SagaId::new(trigger.saga.clone())),
            IndexInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            IndexInput::Trigger(trigger) => Ok(self.deadline.set(
                TimerName::new(REMINDER),
                Duration::from_millis(trigger.after_millis),
                Reminder { note: trigger.note },
            )),
            IndexInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect(FIXTURE_LOCK)
                    .push(reminder.note.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.note),
                }))
            }
        }
    }
}

pub fn partition_count(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a fixture partition count is not zero")
}

/// Host [`IndexSaga`] on `system`, its one source bound by `subscription`.
pub async fn spawn_index_saga<St: SagaOffsetSlot>(
    system: &EventSourceSystem<St>,
    subscription: Subscription,
    partitions: usize,
    fired: &Fired,
) -> SagaHostProxy<IndexSaga> {
    spawn_index_saga_polling(system, subscription, partitions, fired, TEST_POLL_INTERVAL).await
}

/// [`spawn_index_saga`] with the host's poll interval set to `poll`.
pub async fn spawn_index_saga_polling<St: SagaOffsetSlot>(
    system: &EventSourceSystem<St>,
    subscription: Subscription,
    partitions: usize,
    fired: &Fired,
    poll: Duration,
) -> SagaHostProxy<IndexSaga> {
    let fired = Arc::clone(fired);
    SagaManagerProps::<IndexSaga>::new(move |components| IndexSaga {
        deadline: components.deadline(),
        fired: Arc::clone(&fired),
    })
    .observing::<Trigger>(subscription)
    .with_partitions(partition_count(partitions))
    .with_delivery_poll_interval(poll)
    .spawn(system)
    .await
}

/// A system over `store`, a handle of a destination other systems may name too.
pub async fn system_over(store: &InMemoryStore) -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build()
}

// Seeding a saga's own stream

/// The wall-clock reading a schedule fact is anchored to, as the fact holds it.
pub fn now_millis() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// The due time a schedule fact names: its anchor plus its delay.
pub fn due_at_millis(scheduled_at_unix_millis: i64, after: Duration) -> i64 {
    scheduled_at_unix_millis
        + i64::try_from(after.as_millis()).expect("a fixture delay fits in i64 milliseconds")
}

async fn append_one(journal: &Journal, stream: &str, row: AppendingEvent) -> u64 {
    let outcome = journal
        .append(stream, vec![row])
        .await
        .expect("seeding a saga's own stream must succeed");
    *outcome
        .assigned_sequences
        .first()
        .expect("a one-event append is assigned one global sequence")
}

/// One `scheduled` fact on `saga_id`'s own stream, written as the saga would.
#[allow(clippy::too_many_arguments)]
pub async fn seed_scheduled_as(
    journal: &Journal,
    saga_id: &SagaId,
    sequence: u64,
    name: &str,
    after: Duration,
    note: &str,
    scheduled_at_unix_millis: i64,
    message_type: &str,
) -> u64 {
    let payload = <ReminderCodec as Codec<Reminder>>::encode(&Reminder {
        note: note.to_owned(),
    })
    .expect("encoding a Reminder must succeed");
    append_one(
        journal,
        saga_id.as_str(),
        AppendingEvent {
            sequence,
            event_type: SCHEDULE_MARKER,
            payload: encode_schedule_scheduled(
                saga_id,
                &TimerName::new(name),
                after,
                message_type,
                Reminder::CODEC_NAME,
                &payload,
                scheduled_at_unix_millis,
            ),
            occurred_at: jiff::Timestamp::now(),
        },
    )
    .await
}

/// One `scheduled` fact naming this file's own deadline message type.
pub async fn seed_scheduled(
    journal: &Journal,
    saga_id: &SagaId,
    sequence: u64,
    name: &str,
    after: Duration,
    note: &str,
    scheduled_at_unix_millis: i64,
) -> u64 {
    seed_scheduled_as(
        journal,
        saga_id,
        sequence,
        name,
        after,
        note,
        scheduled_at_unix_millis,
        Reminder::TYPE_NAME,
    )
    .await
}

fn encode_schedule_kind(kind: ScheduleKind) -> Bytes {
    Bytes::from(prost::Message::encode_to_vec(&ScheduleMarkerPayload {
        kind: Some(kind),
    }))
}

/// One `cancelled` fact for `name` on `saga_id`'s own stream.
pub async fn seed_cancelled(journal: &Journal, saga_id: &SagaId, sequence: u64, name: &str) -> u64 {
    append_one(
        journal,
        saga_id.as_str(),
        AppendingEvent {
            sequence,
            event_type: SCHEDULE_MARKER,
            payload: encode_schedule_kind(ScheduleKind::Cancelled(CancelledMarkerPayload {
                saga_id: saga_id.as_str().to_owned(),
                name: name.to_owned(),
                cancelled_at_unix_millis: now_millis(),
            })),
            occurred_at: jiff::Timestamp::now(),
        },
    )
    .await
}

/// One `fired` fact for `name` on `saga_id`'s own stream.
pub async fn seed_fired(journal: &Journal, saga_id: &SagaId, sequence: u64, name: &str) -> u64 {
    append_one(
        journal,
        saga_id.as_str(),
        AppendingEvent {
            sequence,
            event_type: SCHEDULE_MARKER,
            payload: encode_schedule_kind(ScheduleKind::Fired(FiredMarkerPayload {
                saga_id: saga_id.as_str().to_owned(),
                name: name.to_owned(),
                fired_at_unix_millis: now_millis(),
            })),
            occurred_at: jiff::Timestamp::now(),
        },
    )
    .await
}

/// The `Ended` lifecycle fact on `saga_id`'s own stream.
pub async fn seed_ended(journal: &Journal, saga_id: &SagaId, sequence: u64) -> u64 {
    append_one(
        journal,
        saga_id.as_str(),
        AppendingEvent {
            sequence,
            event_type: LIFECYCLE_MARKER,
            payload: encode_lifecycle_ended(),
            occurred_at: jiff::Timestamp::now(),
        },
    )
    .await
}

/// One upstream [`Trigger`] at `sequence` of `stream`.
pub async fn append_trigger(journal: &Journal, stream: &str, sequence: u64, trigger: &Trigger) {
    let payload = serde_json::to_vec(trigger)
        .map(Bytes::from)
        .expect("encode Trigger must succeed");
    journal
        .append(
            stream,
            vec![AppendingEvent {
                sequence,
                event_type: Trigger::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Trigger must succeed");
}

pub async fn load_saga_events(journal: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    journal
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

pub fn count_schedule_transition(events: &[LoadedEvent], transition: &str) -> usize {
    events
        .iter()
        .filter_map(schedule_kind_of)
        .filter(|kind| schedule_transition(kind) == transition)
        .count()
}

/// Wait until `saga_id`'s own stream holds a `scheduled` fact, and answer it.
pub async fn wait_until_scheduled(journal: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let events = load_saga_events(journal, saga_id).await;
        if count_schedule_transition(&events, "scheduled") >= 1 {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "the deadline must be persisted on {saga_id:?} before the system stops"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Wait until `fired` holds at least `want` notes, and answer them.
pub async fn wait_until_fired(fired: &Fired, want: usize, label: &str) -> Vec<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let seen = fired.lock().expect(FIXTURE_LOCK).clone();
        if seen.len() >= want {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: wanted {want} firings at `handle`, saw {seen:?}"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

// A whole system that stops

/// Run a system over `store` on a runtime of its own, host [`IndexSaga`] on it
/// with `upstream` as its source, hand it `trigger`, and once the deadline the
/// trigger sets is persisted stop the whole system — manager, instance,
/// resident scheduler, delivery host and index alike — by dropping its runtime.
///
/// Answers what reached `handle` on that system before it stopped. Every
/// process of a system lives on the runtime it was spawned from, so dropping
/// the runtime is the crash a restart begins with: nothing of it is left to
/// fire or to ask for anything.
pub fn run_until_scheduled_then_stop(
    store: &InMemoryStore,
    upstream: &'static str,
    trigger: Trigger,
) -> Vec<String> {
    let store = store.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the stopping system's runtime must build");
        let fired = fired();
        runtime.block_on(async {
            let system = system_over(&store).await;
            let journal = system.persistence().event_store();
            let saga_id = SagaId::new(trigger.saga.clone());
            append_trigger(&journal, upstream, 1, &trigger).await;
            let _host = spawn_index_saga(&system, Subscription::stream(upstream), 1, &fired).await;
            let events = wait_until_scheduled(&journal, &saga_id).await;
            assert_eq!(
                count_schedule_transition(&events, "fired"),
                0,
                "the deadline must still be pending when the system stops, or \
                 there is nothing left for the next system to deliver"
            );
        });
        drop(runtime);
        let seen = fired.lock().expect(FIXTURE_LOCK).clone();
        seen
    })
    .join()
    .expect("the stopping system's thread must not panic")
}

// A journal that records what it is asked, and can fail the index's load

/// Which loads are the index's: the one naming the schedule fact's event type.
///
/// The manager's replay names a stream, its source poller names the trigger's
/// event type and the delivery host names the decision's, so no other consumer
/// of this journal asks for this prefix.
pub fn is_schedule_load(query: &LoadQuery) -> bool {
    query.event_type_prefix.as_ref() == Some(&SCHEDULE_MARKER.to_path())
}

/// The lifecycle-fact counterpart of [`is_schedule_load`].
pub fn is_lifecycle_load(query: &LoadQuery) -> bool {
    query.event_type_prefix.as_ref() == Some(&LIFECYCLE_MARKER.to_path())
}

/// An [`EventStore`] that records every query it is handed and can panic on the
/// index's `nth` schedule load, so the index partition is restarted under
/// supervision.
///
/// Its `Clone` is the handle's: the record and the counter are one region
/// behind every handle, so a fixture keeping a handle reads what the wiring
/// asked for.
#[derive(Clone)]
pub struct ProbeJournal {
    inner: Journal,
    calls: Arc<Mutex<Vec<LoadQuery>>>,
    schedule_loads: Arc<AtomicUsize>,
    panic_on_schedule_load: Option<usize>,
}

impl ProbeJournal {
    pub fn recording(inner: impl Into<Journal>) -> Self {
        Self {
            inner: inner.into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            schedule_loads: Arc::new(AtomicUsize::new(0)),
            panic_on_schedule_load: None,
        }
    }

    pub fn panicking_on_schedule_load(inner: impl Into<Journal>, nth: usize) -> Self {
        Self {
            panic_on_schedule_load: Some(nth),
            ..Self::recording(inner)
        }
    }

    /// Every query this journal was handed, in call order.
    pub fn queries(&self) -> Vec<LoadQuery> {
        self.calls.lock().expect(FIXTURE_LOCK).clone()
    }

    /// The erased handle seeding goes through, so seeded rows land in the
    /// journal the system reads.
    pub fn journal(&self) -> Journal {
        self.inner.clone()
    }
}

#[async_trait]
impl EventStore for ProbeJournal {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let schedule_load = is_schedule_load(&query);
        self.calls.lock().expect(FIXTURE_LOCK).push(query.clone());
        let nth = if schedule_load {
            self.schedule_loads.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        };
        let loaded = self.inner.load(query).await;
        if schedule_load && self.panic_on_schedule_load == Some(nth) {
            panic!("the fixture journal panics on the index's schedule load {nth}");
        }
        loaded
    }
}

pub type ProbeStore = CustomStore<Unprovided, Unprovided, InMemoryOffsetStore>;

/// A system over `journal`, with an in-memory saga-offset slot beside it.
pub async fn probe_system(journal: &ProbeJournal) -> EventSourceSystem<ProbeStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build()
}

/// Wait until `probe` has seen at least `want` queries `select` accepts, and
/// answer those.
pub async fn wait_until_queries(
    probe: &ProbeJournal,
    select: fn(&LoadQuery) -> bool,
    want: usize,
    label: &str,
) -> Vec<LoadQuery> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let queries: Vec<LoadQuery> = probe.queries().into_iter().filter(select).collect();
        if queries.len() >= want {
            return queries;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: saw {} matching load calls, wanted {want}",
            queries.len()
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

// The index's tracing records

/// The message the index reports a finished rebuild with, once per
/// incarnation of a partition.
pub const REBUILT: &str = "saga deadline index rebuilt";

/// The message the index reports each activation request with.
pub const REQUESTED_ACTIVATION: &str = "saga deadline index requested activation";

pub const MESSAGE: &str = "message";
pub const MESSAGE_TYPE: &str = "message_type";
pub const PARTITION: &str = "partition";
pub const COUNT: &str = "count";
pub const SAGA_ID: &str = "saga_id";
pub const TIMER: &str = "timer";
pub const DUE_AT_UNIX_MILLIS: &str = "due_at_unix_millis";

/// One captured record, as its fields.
pub type Fields = BTreeMap<String, String>;

#[derive(Default)]
struct FieldMap(Fields);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // A `&str` recorded through `?` arrives rendered with its quotes; the
        // value is the same either way, and the value is what is asserted.
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
        self.captured.lock().expect(FIXTURE_LOCK).push(fields.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// The process-wide capture of tracing records.
///
/// A process has one global subscriber, and the index runs on runtime-owned
/// threads a thread-scoped one would not see, so a binary that reads the
/// index's records holds exactly one test that hosts a deadline saga: every
/// record it captures with this file's message type is then that test's own.
#[derive(Clone)]
pub struct Capture(Arc<Mutex<Vec<Fields>>>);

static CAPTURE: OnceLock<Capture> = OnceLock::new();

impl Capture {
    pub fn installed() -> Self {
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

    /// Every record with `message` about this file's deadline message type, in
    /// emission order.
    pub fn records(&self, message: &str) -> Vec<Fields> {
        self.0
            .lock()
            .expect(FIXTURE_LOCK)
            .iter()
            .filter(|fields| {
                fields.get(MESSAGE).map(String::as_str) == Some(message)
                    && fields.get(MESSAGE_TYPE).map(String::as_str) == Some(Reminder::TYPE_NAME)
            })
            .cloned()
            .collect()
    }

    pub fn rebuilt(&self) -> Vec<Fields> {
        self.records(REBUILT)
    }

    /// Wait until `select` holds of the records with `message`, and answer them.
    pub async fn wait_for(
        &self,
        message: &str,
        select: impl Fn(&[Fields]) -> bool,
        label: &str,
    ) -> Vec<Fields> {
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            let seen = self.records(message);
            if select(&seen) {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: the `{message}` records captured were {seen:?}"
            );
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }
}

/// The value of `field` in `fields`, as text.
pub fn field<'a>(fields: &'a Fields, field: &str) -> Option<&'a str> {
    fields.get(field).map(String::as_str)
}
