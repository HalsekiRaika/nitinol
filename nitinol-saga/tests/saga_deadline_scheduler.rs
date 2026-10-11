//! End-to-end behaviour of a saga's own time, declared as a `Deadline<M>`
//! component and a `#[subscribe(deadline)]` variant of its alphabet.
//!
//! ```gherkin
//!   Scenario: a deadline from setting to firing
//!     Given a saga with a #[subscribe(deadline)] variant and a Deadline<M>
//!     When handle returns the deadline-setting effect in the same batch as its decision
//!     And the deadline elapses
//!     Then the firing message reaches the deadline arm of the same handle
//! ```
//!
//! The data flow exercised: `Deadline::set` / `Deadline::cancel` → interpreter
//! → resident `SchedulerProcess` timer → the deadline arm of `Saga::handle` →
//! `nitinol.saga.schedule` markers on the saga's own `EventStore` stream.
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]` with one `#[subscribe(deadline)]` newtype
//!   variant whose payload type implements `DeadlineMessage` (`type Codec`).
//! - `SagaComponents::deadline::<M>() -> Deadline<M>`, obtainable only from the
//!   producer the spawn builder calls: `Fn(SagaComponents<S>) -> S`.
//! - `Deadline::set(TimerName, Duration, M) -> SagaEffect<E>` and
//!   `Deadline::cancel(TimerName) -> SagaEffect<E>` — effect constructors, so
//!   the fact is written before any timer is armed.
//! - **No scheduler wiring.**  A saga whose alphabet declares a deadline gets
//!   the system's resident `SchedulerProcess` resolved for it at spawn; there is
//!   no `with_scheduler` to call.  That resolution is pinned on its own in
//!   `saga_host_owns_scheduler.rs`.
//! - Schedule markers are appended under the event type
//!   `nitinol.saga.schedule`; which transition a marker records
//!   (`scheduled` / `cancelled` / `fired`) is carried by its payload's `oneof`.
//!
//! If a name differs in the implementation, only the wiring lines below need to
//! be reconciled — the asserted behaviour is the requirement.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_deadline_scheduler`.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_schedule_scheduled, schedule_kind_of, schedule_transition};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps, Subscription,
    TimerName,
};

// Local JSON codec for the domain events — self-contained so this file does not
// couple to the shared `common` module for its domain.

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

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Trigger {
    note: String,
    after_millis: u64,
    cancel: bool,
    with_decision: bool,
}

impl Trigger {
    fn setting(note: &str, after_millis: u64) -> Self {
        Self {
            note: note.to_owned(),
            after_millis,
            cancel: false,
            with_decision: false,
        }
    }

    fn deciding_and_setting(note: &str, after_millis: u64) -> Self {
        Self {
            with_decision: true,
            ..Self::setting(note, after_millis)
        }
    }

    fn cancelling() -> Self {
        Self {
            note: String::new(),
            after_millis: 0,
            cancel: true,
            with_decision: false,
        }
    }
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_it"), TypeName::new("Trigger"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_it"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// The saga's promise to its future self.  It is persisted inside the schedule
/// fact, which is why it names the format that writes it down — and why that
/// format is a `Codec`, like every other persisted value's, rather than serde.
#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    note: String,
}

/// Deliberately *not* a JSON shape: the bytes a `Reminder` is written as are
/// the note itself, so a payload produced by `serde_json` instead of this codec
/// is visibly different on the stream.
struct ReminderCodec;

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
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

/// The whole alphabet this saga reacts to: one external source, and its own
/// time.
#[derive(SagaSubscribe)]
enum TimerInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// How many times `correlate` was asked about a deadline firing.
///
/// Binary-wide, because `correlate` is an associated function with no receiver
/// to hang state off.  Every test in this file requires it to stay at zero: a
/// firing is addressed to the instance that set it and never travels the
/// subscription path correlation answers for.
static CORRELATE_ASKED_ABOUT_A_DEADLINE: AtomicUsize = AtomicUsize::new(0);

/// Correlation rule of [`TimerSaga`]: every test below drives a single timer
/// process against its own stores, so one id names the instance a `Trigger`
/// belongs to.
const TIMER_SAGA_ID: &str = "deadline-fires-1";

struct TimerSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for TimerSaga {
    type Subscribe = TimerInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            TimerInput::Trigger(_) => Some(SagaId::new(TIMER_SAGA_ID)),
            TimerInput::Reminder(_) => {
                CORRELATE_ASKED_ABOUT_A_DEADLINE.fetch_add(1, Ordering::SeqCst);
                None
            }
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            TimerInput::Trigger(trigger) if trigger.cancel => {
                Ok(self.deadline.cancel(TimerName::new("reminder")))
            }
            TimerInput::Trigger(trigger) => {
                let set = self.deadline.set(
                    TimerName::new("reminder"),
                    Duration::from_millis(trigger.after_millis),
                    Reminder {
                        note: trigger.note.clone(),
                    },
                );
                if trigger.with_decision {
                    return Ok(SagaEffect::persist(Recorded {
                        label: format!("decided:{}", trigger.note),
                    })
                    .combine(set));
                }
                Ok(set)
            }
            TimerInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect("fired mutex")
                    .push(reminder.note.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.note),
                }))
            }
        }
    }
}

/// A saga whose deadline arm re-sets the deadline under the **same**
/// `TimerName` exactly once, controlled by the payload rather than by a
/// per-instance counter so the behaviour is incarnation-independent.
struct OnceRescheduleSaga {
    deadline: Deadline<GenerationReminder>,
    fires: Arc<Mutex<Vec<String>>>,
}

/// Payload for the marker-ordering regression saga.  `generation` controls
/// whether the deadline arm re-sets: generation 0 re-sets under the same name
/// with generation 1; generation 1 is a no-op.
#[derive(Clone, Debug, PartialEq)]
struct GenerationReminder {
    note: String,
    generation: u32,
}

struct GenerationReminderCodec;

impl Codec<GenerationReminder> for GenerationReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &GenerationReminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(format!("{}|{}", value.generation, value.note)))
    }

    fn decode(payload: &[u8]) -> Result<GenerationReminder, Self::Error> {
        let text = String::from_utf8_lossy(payload).into_owned();
        let (generation, note) = text.split_once('|').unwrap_or(("0", text.as_str()));
        Ok(GenerationReminder {
            generation: generation.parse().unwrap_or(0),
            note: note.to_owned(),
        })
    }
}

impl DeadlineMessage for GenerationReminder {
    const TYPE_NAME: &'static str = "generation_reminder";
    const CODEC_NAME: &'static str = "generation-reminder-codec";
    type Codec = GenerationReminderCodec;
}

#[derive(SagaSubscribe)]
enum RepeatInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Repeat(GenerationReminder),
}

/// Correlation rule of [`OnceRescheduleSaga`]: both incarnations of the
/// regression below are the same process instance, so they answer alike.
const ONCE_RESCHEDULE_SAGA_ID: &str = "deadline-arch004-1";

#[async_trait]
impl Saga for OnceRescheduleSaga {
    type Subscribe = RepeatInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            RepeatInput::Trigger(_) => Some(SagaId::new(ONCE_RESCHEDULE_SAGA_ID)),
            RepeatInput::Repeat(_) => {
                CORRELATE_ASKED_ABOUT_A_DEADLINE.fetch_add(1, Ordering::SeqCst);
                None
            }
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            RepeatInput::Trigger(trigger) => Ok(self.deadline.set(
                TimerName::new("repeat"),
                Duration::from_millis(trigger.after_millis),
                GenerationReminder {
                    note: trigger.note,
                    generation: 0,
                },
            )),
            RepeatInput::Repeat(reminder) => {
                self.fires
                    .lock()
                    .expect("fires mutex")
                    .push(reminder.note.clone());
                if reminder.generation == 0 {
                    return Ok(self.deadline.set(
                        TimerName::new("repeat"),
                        Duration::from_millis(300),
                        GenerationReminder {
                            note: format!("{}-2nd", reminder.note),
                            generation: 1,
                        },
                    ));
                }
                Ok(SagaEffect::empty())
            }
        }
    }
}

// Stores

#[derive(Clone, Debug)]
struct RecordedRow {
    event_type: EventType,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedRow>>>>;

/// Records one entry per successful `append`, preserving the batch boundary —
/// the only place "the decision and its deadline reached the store together" is
/// observable.
struct RecordingStore {
    inner: InMemoryEventStore,
    batches: Batches,
}

#[async_trait]
impl EventStore for RecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let recorded: Vec<RecordedRow> = events
            .iter()
            .map(|event| RecordedRow {
                event_type: event.event_type,
            })
            .collect();
        let outcome = self.inner.append(key, events).await?;
        self.batches
            .lock()
            .expect("recorded batches must not be poisoned")
            .push(recorded);
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Helpers

/// The transitions recorded on this stream, in the order they were written.
fn schedule_transitions(events: &[LoadedEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(schedule_kind_of)
        .map(|kind| schedule_transition(&kind))
        .collect()
}

fn count_schedule_transition(events: &[LoadedEvent], transition: &'static str) -> usize {
    schedule_transitions(events)
        .into_iter()
        .filter(|recorded| *recorded == transition)
        .count()
}

async fn append_trigger(
    store: &Arc<dyn EventStore>,
    stream_key: &str,
    sequence: u64,
    trigger: Trigger,
) {
    let payload = serde_json::to_vec(&trigger)
        .map(Bytes::from)
        .expect("encode Trigger must succeed");
    store
        .append(
            stream_key,
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

async fn wait_until<F: Fn() -> bool>(deadline: Duration, cond: F) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Spawn a [`TimerSaga`] with no scheduler wiring: the alphabet declaring a
/// deadline variant is the whole of the declaration, and the host resolves the
/// resident timer core from it.
async fn spawn_timer_saga(
    ps: &ProcessSystem,
    saga_id: &SagaId,
    saga_store: &Arc<dyn EventStore>,
    upstream_store: &Arc<dyn EventStore>,
    upstream_key: &str,
    fired: &Arc<Mutex<Vec<String>>>,
) -> nitinol_saga::SagaProxy<TimerSaga> {
    let fired = Arc::clone(fired);
    SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(saga_store), move |components| {
        TimerSaga {
            deadline: components.deadline(),
            fired: Arc::clone(&fired),
        }
    })
    .subscribed_to::<Trigger>(Subscription::stream(upstream_store, upstream_key))
    .spawn(ps)
    .await
}

fn assert_correlate_was_never_asked_about_a_deadline() {
    assert_eq!(
        CORRELATE_ASKED_ABOUT_A_DEADLINE.load(Ordering::SeqCst),
        0,
        "a deadline firing is addressed to the instance that set it, so the \
         framework must never route one through `correlate`"
    );
}

// Tests

/// Given a saga whose handle sets a `Reminder` deadline a short way out,
/// When the deadline elapses,
/// Then the firing reaches the deadline arm of the same `handle`, carrying the
/// message that was set.
#[tokio::test]
async fn a_deadline_fires_into_the_deadline_arm_of_handle() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-1",
        1,
        Trigger::setting("hello", 150),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-1",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert!(ok, "the deadline must fire within 5s; fired={seen:?}");
    assert_eq!(
        seen,
        vec!["hello".to_owned()],
        "the deadline arm must receive exactly the message that was set"
    );
    assert_correlate_was_never_asked_about_a_deadline();
}

/// Acceptance scenario: the deadline is set in the same batch as the decision.
///
/// Given a handle that returns its decision combined with a deadline-setting
/// effect,
/// When the result is persisted,
/// Then one append carries both the decision and the `scheduled` marker:
/// neither is ever in the store without the other.
#[tokio::test]
async fn a_decision_and_the_deadline_it_sets_reach_the_store_in_one_append() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-batch",
        1,
        Trigger::deciding_and_setting("batched", 150),
    )
    .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-batch",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;
    assert!(ok, "the batched deadline must still fire");

    let recorded = batches
        .lock()
        .expect("recorded batches must not be poisoned")
        .clone();
    let opening = recorded
        .first()
        .expect("the decision's batch must have been recorded");
    let types: Vec<String> = opening
        .iter()
        .map(|row| row.event_type.to_string())
        .collect();

    assert!(
        types.iter().any(|t| t == &Recorded::EVENT_TYPE.to_string())
            && types
                .iter()
                .any(|t| t == &common::SCHEDULE_MARKER.to_string()),
        "the decision and the deadline fact must share one atomic batch, so no \
         crash can leave a timer armed against a decision no store holds; the \
         opening batch held {types:?}"
    );
}

/// Given a deadline and its subsequent firing,
/// Then the saga's own stream carries a `scheduled` marker (on setting) and a
/// `fired` marker (on delivery), both under the `nitinol.saga.schedule` type
/// key.
#[tokio::test]
async fn setting_and_firing_persist_schedule_markers_on_the_saga_stream() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-2",
        1,
        Trigger::setting("mark", 150),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-2",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;
    assert!(ok, "the deadline must fire so a `fired` marker is written");

    // The `fired:` decision is the barrier for the post-fire append: it is the
    // effect the deadline arm returned, so once it is on the stream the fire's
    // own marker is too.
    let deadline_for_rows = Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        if count_schedule_transition(&events, "fired") >= 1 {
            break events;
        }
        assert!(
            Instant::now() < deadline_for_rows,
            "timed out waiting for the `fired` marker; stream held {:?}",
            schedule_transitions(&events)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        count_schedule_transition(&events, "scheduled"),
        1,
        "setting a deadline must append exactly one \
         `nitinol.saga.schedule.scheduled` marker"
    );
    assert_eq!(
        count_schedule_transition(&events, "fired"),
        1,
        "firing must append exactly one `nitinol.saga.schedule.fired` marker"
    );
    assert_eq!(
        count_schedule_transition(&events, "cancelled"),
        0,
        "no cancellation occurred, so no `cancelled` marker may be written"
    );
}

/// `startSingleTimer` semantics: re-setting the same `TimerName` auto-cancels
/// the earlier registration, so only the latest deadline fires.
#[tokio::test]
async fn resetting_the_same_name_auto_cancels_the_earlier_deadline() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-3",
        1,
        Trigger::setting("stale", 1_500),
    )
    .await;
    append_trigger(
        &upstream_store,
        "deadline-upstream-3",
        2,
        Trigger::setting("fresh", 150),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-3",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;
    assert!(ok, "the re-set (fresh) deadline must fire");

    // Wait past the stale deadline's original due time to prove it was cancelled.
    tokio::time::sleep(Duration::from_millis(1_800)).await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert_eq!(
        seen,
        vec!["fresh".to_owned()],
        "only the latest deadline under a re-used name may fire (the stale one \
         is auto-cancelled)"
    );
}

/// `Deadline::cancel(name)` cancels a pending deadline so it never fires, and
/// records a `cancelled` marker on the saga stream.
#[tokio::test]
async fn cancel_prevents_the_deadline_from_firing() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-4",
        1,
        Trigger::setting("doomed", 600),
    )
    .await;
    append_trigger(
        &upstream_store,
        "deadline-upstream-4",
        2,
        Trigger::cancelling(),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-4",
        &fired,
    )
    .await;

    // The `cancelled` marker is the barrier for the negative claim below: it is
    // appended by the second trigger, so once it is on the stream both triggers
    // have been handled and the only thing left to outlast is the doomed
    // deadline's own due window.  A bare sleep would assert `cancelled == 1`
    // against a stream the poller may not have reached yet.
    let deadline_for_rows = Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        if count_schedule_transition(&events, "cancelled") >= 1 {
            break events;
        }
        assert!(
            Instant::now() < deadline_for_rows,
            "timed out waiting for the `cancelled` marker; stream held {:?}",
            schedule_transitions(&events)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        count_schedule_transition(&events, "cancelled"),
        1,
        "cancel must append exactly one `nitinol.saga.schedule.cancelled` marker"
    );

    // The cancel was handled after the deadline was set, so outlasting the full
    // 600ms due window from here proves the timer was disarmed rather than late.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert!(
        seen.is_empty(),
        "a cancelled deadline must never fire; fired={seen:?}"
    );

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_schedule_transition(&events, "fired"),
        0,
        "a cancelled deadline must not produce a `fired` marker"
    );
}

/// A fresh incarnation started against a store that already holds an unfired
/// `scheduled` marker must re-arm the deadline from replay and fire it — even
/// though this incarnation never receives the upstream trigger itself.
///
/// The seeded payload is written with `ReminderCodec`, which is also what the
/// derive has to read the schedule fact back through: a replay that reached for
/// serde instead would not recover the note.
#[tokio::test]
async fn replay_rearms_an_unfired_deadline_and_fires_it() {
    let ps = ProcessSystem::new().await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let name = TimerName::new("reminder");
    let survivor = Reminder {
        note: "survivor".to_owned(),
    };
    let payload = <ReminderCodec as Codec<Reminder>>::encode(&survivor)
        .expect("encoding a Reminder must succeed");

    // The state a crash leaves behind: an unfired schedule fact whose due time
    // is still ahead.
    saga_store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: common::SCHEDULE_MARKER,
                payload: encode_schedule_scheduled(
                    &saga_id,
                    &name,
                    Duration::from_millis(400),
                    Reminder::TYPE_NAME,
                    Reminder::CODEC_NAME,
                    &payload,
                    jiff::Timestamp::now().as_millisecond(),
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding an unfired schedule fact must succeed");

    // An empty upstream, so the only way this incarnation can learn about the
    // deadline is replay.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-5-empty",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(8), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert!(
        ok,
        "the replaying incarnation must re-arm and fire the unfired deadline; \
         fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["survivor".to_owned()],
        "the re-armed deadline must carry the message the schedule fact holds, \
         read back through the codec the message type names"
    );
}

/// A delay far past the horizon one revolution of the retired hashed wheel
/// covered (512 slots x 10 ms = 5.12 s).  A wheel reached such a due time only
/// by counting revolutions per entry; a deadline-ordered timer waits for the
/// due time itself.
const BEYOND_OLD_WHEEL_HORIZON: u64 = 6_000;

/// Given a deadline whose delay is longer than the retired wheel's single
/// revolution,
/// when that delay elapses,
/// then it fires once and records one `fired` marker.
#[tokio::test]
async fn a_deadline_beyond_the_old_wheel_horizon_fires() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-horizon",
        1,
        Trigger::setting("beyond-horizon", BEYOND_OLD_WHEEL_HORIZON),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TIMER_SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let set_at = Instant::now();
    let _proxy = spawn_timer_saga(
        &ps,
        &saga_id,
        &saga_store,
        &upstream_store,
        "deadline-upstream-horizon",
        &fired,
    )
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(20), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;

    let waited = set_at.elapsed();
    let seen = fired.lock().expect("fired mutex").clone();
    assert!(
        ok,
        "a {BEYOND_OLD_WHEEL_HORIZON}ms deadline must still fire; fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["beyond-horizon".to_owned()],
        "the long deadline must deliver its own message, exactly once"
    );
    assert!(
        waited >= Duration::from_millis(BEYOND_OLD_WHEEL_HORIZON),
        "the deadline fired after {waited:?}, before its \
         {BEYOND_OLD_WHEEL_HORIZON}ms due time"
    );
}

/// Regression: when the deadline arm re-sets under the same `TimerName`, the
/// second incarnation (replay) must still see the re-set deadline as pending
/// and eventually fire it.
///
/// Without the ordering fix the stream reads `Scheduled → (new) Scheduled →
/// Fired`, and replay's `Fired` handling removes the new schedule by name,
/// leaving it invisible to the second incarnation.  With it the order is
/// `Scheduled → Fired → (new) Scheduled`.
#[tokio::test]
async fn a_deadline_reset_from_its_own_firing_survives_replay() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-arch004",
        1,
        Trigger::setting("first", 150),
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(ONCE_RESCHEDULE_SAGA_ID);

    let fires_1 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fires_1_producer = Arc::clone(&fires_1);
    let _proxy_1 = SagaProps::<OnceRescheduleSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| OnceRescheduleSaga {
            deadline: components.deadline(),
            fires: Arc::clone(&fires_1_producer),
        },
    )
    .subscribed_to::<Trigger>(Subscription::stream(
        &upstream_store,
        "deadline-upstream-arch004",
    ))
    .spawn(&ps)
    .await;

    let fires_1_check = Arc::clone(&fires_1);
    let ok = wait_until(Duration::from_secs(5), || {
        !fires_1_check.lock().expect("fires mutex").is_empty()
    })
    .await;
    assert!(ok, "incarnation #1 must fire the first deadline");

    // The three markers landing is the barrier for the ordering assertion.
    let deadline_for_rows = Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        if schedule_transitions(&events).len() >= 3 {
            break events;
        }
        assert!(
            Instant::now() < deadline_for_rows,
            "timed out waiting for three schedule markers; stream held {:?}",
            schedule_transitions(&events)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled", "fired", "scheduled"],
        "the stream must hold the re-set deadline after the fire it followed, \
         and nothing between"
    );

    // Incarnation #2 shares the store but subscribes to an empty upstream, so
    // it can only learn about the re-set deadline via replay.
    let upstream_empty: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let fires_2 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fires_2_producer = Arc::clone(&fires_2);
    let _proxy_2 = SagaProps::<OnceRescheduleSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| OnceRescheduleSaga {
            deadline: components.deadline(),
            fires: Arc::clone(&fires_2_producer),
        },
    )
    .subscribed_to::<Trigger>(Subscription::stream(
        &upstream_empty,
        "deadline-upstream-arch004-empty",
    ))
    .spawn(&ps)
    .await;

    let fires_2_check = Arc::clone(&fires_2);
    let ok = wait_until(Duration::from_secs(8), || {
        !fires_2_check.lock().expect("fires mutex").is_empty()
    })
    .await;

    let seen_2 = fires_2.lock().expect("fires mutex").clone();
    assert!(
        ok,
        "incarnation #2 must re-arm and fire the re-set deadline after replay; \
         fired={seen_2:?}"
    );
    assert_eq!(
        seen_2,
        vec!["first-2nd".to_owned()],
        "the re-set deadline must carry the message its own arm set, not the \
         original"
    );
    assert_correlate_was_never_asked_about_a_deadline();
}
