//! A deadline firing is settled only once its decision is durable.
//!
//! ```gherkin
//!   Scenario: a decision that never became durable is tried again
//!     Given a saga with a pending deadline that fires
//!     And the append its decision needed is refused until it gives up
//!     When the saga is restarted and replays
//!     Then the deadline is pending again, re-armed, and reaches handle again
//!     And no fired marker for it is on the stream
//!
//!   Scenario: a decision that became durable is not fired again
//!     Given a firing whose decision was persisted
//!     Then the fired marker is on the stream after the decision
//!     And a restart finds the deadline neither pending nor re-armed
//!
//!   Scenario: End leaves no fired marker
//!     Given a firing whose deadline arm returns End
//!     Then the stream holds the Ended fact and no fired marker
//!
//!   Scenario: a firing that decides nothing is settled once
//!     Given a firing handle refuses, cannot read, or answers None
//!     Then the fired marker is the whole of what the firing leaves
//!     And a restart re-arms nothing and never calls handle again
//!
//!   Scenario: a fired marker that never landed is delivered again
//!     Given a firing whose fired marker's append is refused
//!     When the saga is restarted and replays
//!     Then the deadline is still pending and reaches handle again
//!
//!   Scenario: a reset judged by what landed, not by what was asked for
//!     Given a deadline arm that re-sets the same TimerName
//!     And the append that re-set it is refused until it gives up
//!     Then no fired marker is written either, and the original deadline
//!     survives to be re-armed with the message it was set with
//! ```
//!
//! # Why the order is the contract
//!
//! Every other input a saga takes settles behind its decision: the consumed
//! position rides the same append, so an input whose decision is in no store is
//! not consumed and comes round again. A deadline firing has no position — it
//! was addressed to the instance, not read from a stream — so the row that
//! settles it is its own `fired` marker, and the marker has to land *after* the
//! decision for the same reason. Written before it, a decision that never
//! became durable leaves a deadline replay folds out of the journal for good:
//! the firing is lost, the decision with it, and the saga waits on an input
//! that is not coming.
//!
//! Firing is therefore at-least-once. The window this file leaves open on
//! purpose is the other side of it — a stop between the decision landing and
//! the `fired` marker landing re-delivers the firing — which
//! [`a_firing_whose_fired_marker_never_landed_is_re_delivered_after_a_restart`]
//! pins as tolerated.
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]` with one `#[subscribe(deadline)]` newtype
//!   variant, `SagaComponents::deadline::<M>()`, `Deadline::set`, and
//!   `Deadline::pending()` — see `saga_deadline_scheduler.rs` and
//!   `saga_deadline_pending_view.rs` for the full assumed surface.
//! - Schedule markers are appended under `nitinol.saga.schedule`, with the
//!   transition (`scheduled` / `cancelled` / `fired`) in the payload's `oneof`.
//! - The marker-ordering half of the same contract — a same-name reset writing
//!   no `fired` marker at all — is held by
//!   `saga_deadline_scheduler.rs::a_deadline_reset_from_its_own_firing_survives_replay`.
//!   What this file adds there is the boundary: the reset whose fact never
//!   landed, and the reset of a *different* name.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_deadline_fired_after_decision`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decode_schedule_kind, encode_schedule_scheduled, lifecycle_fact, lifecycle_kind_of,
    schedule_kind_of, schedule_transition, JsonCodec, ScheduleKind, CONSUMED_MARKER,
    SCHEDULE_MARKER,
};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    Deadline, DeadlineMessage, PendingDeadline, Saga, SagaContext, SagaEffect, SagaId, SagaProps,
    Subscription, TimerName,
};

// Domain

/// Asks the saga what the journal says is pending.
///
/// A firing is addressed to the instance rather than observed through a
/// subscription, so the pending view needs an input of its own to be read back
/// through `Saga::handle` — the one place a `Deadline<M>` is reachable.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Probe;

impl Event for Probe {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_settle"), TypeName::new("Probe"));
    type Codec = JsonCodec;
}

/// The saga's own decision — the row a `fired` marker must never precede.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_settle"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// What the deadline arm does with a firing.
///
/// Carried inside the persisted message rather than counted on the instance, so
/// a firing re-delivered after a restart asks for exactly what the first one
/// did — which is what makes the two halves of a restart comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Persist one decision row.
    Decide,
    /// Re-set the deadline under the *same* name.
    Reset,
    /// Set a deadline under a *different* name.
    SetOther,
    /// Terminate the saga.
    Finish,
    /// Refuse the firing.
    Refuse,
    /// Decide nothing at all.
    Nothing,
}

impl Plan {
    fn tag(self) -> &'static str {
        match self {
            Plan::Decide => "decide",
            Plan::Reset => "reset",
            Plan::SetOther => "set-other",
            Plan::Finish => "finish",
            Plan::Refuse => "refuse",
            Plan::Nothing => "nothing",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "decide" => Some(Plan::Decide),
            "reset" => Some(Plan::Reset),
            "set-other" => Some(Plan::SetOther),
            "finish" => Some(Plan::Finish),
            "refuse" => Some(Plan::Refuse),
            "nothing" => Some(Plan::Nothing),
            _ => None,
        }
    }
}

/// The value this saga promises its future self.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Reminder {
    plan: Plan,
    label: String,
}

#[derive(Debug)]
struct UnreadableReminder(String);

impl std::fmt::Display for UnreadableReminder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a `plan|label` reminder: {:?}", self.0)
    }
}

impl std::error::Error for UnreadableReminder {}

/// `plan|label` — deliberately not a serde shape, so the bytes inside a
/// schedule fact are visibly the ones this codec wrote.
struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = UnreadableReminder;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(format!("{}|{}", value.plan.tag(), value.label)))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        let text = String::from_utf8_lossy(payload).into_owned();
        let (tag, label) = text
            .split_once('|')
            .ok_or_else(|| UnreadableReminder(text.clone()))?;
        let plan = Plan::from_tag(tag).ok_or_else(|| UnreadableReminder(text.clone()))?;
        Ok(Reminder {
            plan,
            label: label.to_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "settle-reminder";
    const CODEC_NAME: &'static str = "settle-reminder-codec";
    type Codec = ReminderCodec;
}

/// The whole alphabet this saga reacts to: one observed source, and its own
/// time.
#[derive(SagaSubscribe)]
enum FiringInput {
    Probe(Probe),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

#[derive(Debug)]
struct RefusedFiring(String);

impl std::fmt::Display for RefusedFiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the deadline arm refused the firing {:?}", self.0)
    }
}

impl std::error::Error for RefusedFiring {}

/// Correlation rule of [`FiringSaga`]: every test below drives one instance
/// against stores of its own, so a single id names the instance a `Probe`
/// belongs to.
const SAGA_ID: &str = "deadline-settle-saga";

/// The timer every seeded firing is registered under.
const TIMER: &str = "remind";

/// The timer [`Plan::SetOther`] registers instead.
const OTHER_TIMER: &str = "other";

/// Short, so a seeded deadline is delivered promptly — and so the same delay is
/// long past by the time a restart re-arms it, which is what makes the second
/// incarnation's firing immediate.
const SEED_DELAY: Duration = Duration::from_millis(150);

/// Long enough that a deadline one of these tests *sets* never comes due while
/// the test is still asserting on the stream it was written to.
const PARKED: Duration = Duration::from_secs(30);

type Fires = Arc<Mutex<Vec<String>>>;
/// What one probe reported: the pending view as `(name, due_at)` pairs.
type Reports = Arc<Mutex<Vec<Vec<(String, jiff::Timestamp)>>>>;

struct FiringSaga {
    deadline: Deadline<Reminder>,
    fires: Fires,
    reports: Reports,
}

#[async_trait]
impl Saga for FiringSaga {
    type Subscribe = FiringInput;
    type Event = Recorded;
    type Error = RefusedFiring;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            FiringInput::Probe(_) => Some(SagaId::new(SAGA_ID)),
            // A firing is addressed to the instance that set it, and never
            // travels the subscription path correlation answers for.
            FiringInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            FiringInput::Probe(Probe) => {
                self.reports
                    .lock()
                    .expect("reports mutex")
                    .push(names_and_due(&self.deadline.pending()));
                Ok(SagaEffect::None)
            }
            FiringInput::Reminder(reminder) => {
                // Recorded before the arm decides anything, so a refusal is
                // still observable as "handle saw this firing".
                self.fires
                    .lock()
                    .expect("fires mutex")
                    .push(reminder.label.clone());
                match reminder.plan {
                    Plan::Decide => Ok(SagaEffect::persist(Recorded {
                        label: reminder.label,
                    })),
                    Plan::Reset => Ok(self.deadline.set(
                        TimerName::new(TIMER),
                        PARKED,
                        Reminder {
                            plan: Plan::Nothing,
                            label: format!("{}-2nd", reminder.label),
                        },
                    )),
                    Plan::SetOther => Ok(self.deadline.set(
                        TimerName::new(OTHER_TIMER),
                        PARKED,
                        Reminder {
                            plan: Plan::Nothing,
                            label: format!("{}-other", reminder.label),
                        },
                    )),
                    Plan::Finish => Ok(SagaEffect::End),
                    Plan::Refuse => Err(RefusedFiring(reminder.label)),
                    Plan::Nothing => Ok(SagaEffect::None),
                }
            }
        }
    }
}

// Stores

/// Which append an armed [`RejectingStore`] refuses.
///
/// Each models one half of the firing path giving up: the decision that cannot
/// be persisted, the settlement that cannot be persisted, and the re-set
/// schedule fact that cannot be persisted.
#[derive(Clone, Copy, Debug)]
enum Reject {
    /// Any append carrying the saga's own decision row.
    Decision,
    /// Any append carrying a `fired` schedule marker.
    FiredMarker,
    /// Any append carrying a `scheduled` schedule marker.
    ScheduledMarker,
}

/// Refuses the one append named by `reject` while `armed` is set, and accepts
/// everything else.
///
/// Per row rather than per batch key: the rows the firing path writes are
/// several separate appends, and which one is refused is the whole subject of
/// these tests.
struct RejectingStore {
    inner: InMemoryEventStore,
    reject: Reject,
    armed: Arc<AtomicBool>,
}

/// The transition a row about to be appended records, or `None` when the row is
/// not a schedule marker.
fn appending_schedule_kind(event: &AppendingEvent) -> Option<ScheduleKind> {
    (event.event_type == SCHEDULE_MARKER).then(|| decode_schedule_kind(&event.payload))
}

impl RejectingStore {
    fn refuses(&self, events: &[AppendingEvent]) -> bool {
        events.iter().any(|event| match self.reject {
            Reject::Decision => event.event_type == Recorded::EVENT_TYPE,
            Reject::FiredMarker => {
                matches!(appending_schedule_kind(event), Some(ScheduleKind::Fired(_)))
            }
            Reject::ScheduledMarker => matches!(
                appending_schedule_kind(event),
                Some(ScheduleKind::Scheduled(_))
            ),
        })
    }

    fn refusal(&self) -> &'static str {
        match self.reject {
            Reject::Decision => "injected: the saga's decision never landed",
            Reject::FiredMarker => "injected: the `fired` marker never landed",
            Reject::ScheduledMarker => "injected: the `scheduled` marker never landed",
        }
    }
}

#[async_trait]
impl EventStore for RejectingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.armed.load(Ordering::SeqCst) && self.refuses(&events) {
            return Err(AppendError::Backend(self.refusal().into()));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Projections

/// The pending view as this file asserts against it.
fn names_and_due(pending: &[PendingDeadline]) -> Vec<(String, jiff::Timestamp)> {
    let mut projected: Vec<(String, jiff::Timestamp)> = pending
        .iter()
        .map(|entry| (entry.name.as_str().to_owned(), entry.due_at))
        .collect();
    projected.sort();
    projected
}

/// Just the timers a report names, for the claims that are about *which*
/// deadlines are pending rather than when they are due.
fn names(report: &[(String, jiff::Timestamp)]) -> Vec<&str> {
    report.iter().map(|(name, _)| name.as_str()).collect()
}

/// Every row of the saga's own stream, as the fact it records.
///
/// Read row by row: "no `fired` marker" has to be a claim about rows, and where
/// the marker sits among them is itself the contract here.
fn stream_facts(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            if let Some(kind) = schedule_kind_of(event) {
                return schedule_transition(&kind).to_owned();
            }
            if let Some(kind) = lifecycle_kind_of(event) {
                return lifecycle_fact(&kind).to_owned();
            }
            if event.event_type == CONSUMED_MARKER {
                return "consumed".to_owned();
            }
            if event.event_type == Recorded::EVENT_TYPE {
                return "recorded".to_owned();
            }
            event.event_type.to_string()
        })
        .collect()
}

/// The facts a test names, so an expectation reads as the stream does.
fn facts(expected: &[&str]) -> Vec<String> {
    expected.iter().map(|fact| (*fact).to_owned()).collect()
}

/// The schedule transitions on this stream, in write order.
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

/// The timer each `fired` marker on this stream names.
///
/// A settlement that named the wrong timer would remove a deadline nothing
/// fired, which counting markers alone cannot tell apart.
fn fired_timer_names(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(schedule_kind_of)
        .filter_map(|kind| match kind {
            ScheduleKind::Fired(marker) => Some(marker.name),
            _ => None,
        })
        .collect()
}

/// Where a fact sits on the stream, so "after the decision" is asserted as an
/// order rather than as a whole expected stream the framework's own bookkeeping
/// would make brittle.
fn position_of(stream: &[String], fact: &str) -> Option<usize> {
    stream.iter().position(|recorded| recorded == fact)
}

// Fixtures

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Seed the state a stop leaves behind: one unfired schedule fact for [`TIMER`],
/// due [`SEED_DELAY`] after the anchor it returns.
///
/// `message_type` is handed in rather than taken from [`Reminder`] so a test
/// whose subject is a firing nobody can read can name an identity the current
/// declaration disagrees with.
async fn seed_firing(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    reminder: Reminder,
    message_type: &str,
) -> i64 {
    let payload = <ReminderCodec as Codec<Reminder>>::encode(&reminder)
        .expect("encoding a Reminder must succeed");
    let anchor = jiff::Timestamp::now().as_millisecond();
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: SCHEDULE_MARKER,
                payload: encode_schedule_scheduled(
                    saga_id,
                    &TimerName::new(TIMER),
                    SEED_DELAY,
                    message_type,
                    Reminder::CODEC_NAME,
                    &payload,
                    anchor,
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding an unfired schedule fact must succeed");
    anchor
}

/// When a deadline recorded with `after` from the anchor `anchor_unix_millis` is
/// due — the pending view's own arithmetic, so a test can name the instant the
/// seeded fact implies.
fn due(anchor_unix_millis: i64, after: Duration) -> jiff::Timestamp {
    let millis = anchor_unix_millis
        + i64::try_from(after.as_millis()).expect("the test's delays fit in an i64");
    jiff::Timestamp::from_millisecond(millis).expect("the computed due time is representable")
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Long enough for a firing the implementation should not have armed to arrive
/// if it did: the seeded deadline is already overdue when a restart replays, so
/// a re-armed one fires at once rather than after its original delay.
const QUIET_WINDOW: Duration = Duration::from_millis(600);

/// Wait until the saga's own stream holds at least `expected` markers of
/// `transition`, answering with the rows loaded when it did.
async fn wait_for_transition(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    transition: &'static str,
    expected: usize,
) -> Vec<LoadedEvent> {
    let barrier = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_schedule_transition(&events, transition) >= expected {
            return events;
        }
        assert!(
            Instant::now() < barrier,
            "timed out waiting for {expected} `{transition}` marker(s) on the \
             saga's own stream; it held {:?}",
            stream_facts(&events)
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Wait until the saga's own stream carries the durable `Ended` fact.
async fn wait_for_ended(store: &Arc<dyn EventStore>, saga_id: &SagaId) {
    let barrier = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.iter().filter_map(lifecycle_kind_of).count() >= 1 {
            return;
        }
        assert!(
            Instant::now() < barrier,
            "timed out waiting for the `Ended` fact; the stream held {:?}",
            stream_facts(&events)
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One live incarnation of the saga: the upstream it answers probes from, and
/// what it recorded.
///
/// Held whole by each test — dropping the system or the proxy would take the
/// process down — and started on a `ProcessSystem` of its own, so the resident
/// scheduler a restart arms its deadlines with is as empty as a fresh process's:
/// nothing an earlier incarnation registered can fire into this one.
struct Incarnation {
    _system: ProcessSystem,
    _proxy: nitinol_saga::SagaProxy<FiringSaga>,
    upstream: Arc<dyn EventStore>,
    upstream_key: &'static str,
    probes_sent: AtomicU64,
    fires: Fires,
    reports: Reports,
}

impl Incarnation {
    async fn start(
        saga_store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        upstream_key: &'static str,
    ) -> Self {
        let system = ProcessSystem::new().await;
        // An upstream of its own: a position this incarnation records is
        // keyed by the stream it read, so sharing one key with an earlier
        // incarnation would make its probe look already consumed.
        let upstream: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let fires: Fires = Arc::new(Mutex::new(Vec::new()));
        let reports: Reports = Arc::new(Mutex::new(Vec::new()));
        let fires_for_saga = Arc::clone(&fires);
        let reports_for_saga = Arc::clone(&reports);
        let proxy = SagaProps::<FiringSaga>::new(
            saga_id.clone(),
            Arc::clone(saga_store),
            move |components| FiringSaga {
                deadline: components.deadline(),
                fires: Arc::clone(&fires_for_saga),
                reports: Arc::clone(&reports_for_saga),
            },
        )
        .subscribed_to::<Probe>(Subscription::stream(&upstream, upstream_key))
        .spawn(&system)
        .await;
        Self {
            _system: system,
            _proxy: proxy,
            upstream,
            upstream_key,
            probes_sent: AtomicU64::new(0),
            fires: Arc::clone(&fires),
            reports: Arc::clone(&reports),
        }
    }

    /// Ask this incarnation what the journal left pending.
    ///
    /// Appended only after whatever precedes it has already been observed from
    /// inside `handle`, so the mailbox hands the probe over *after* that turn
    /// is finished: the report it answers with is the barrier every negative
    /// claim behind it rests on.
    async fn probe(&self) -> Vec<(String, jiff::Timestamp)> {
        let sent = self.probes_sent.fetch_add(1, Ordering::SeqCst) + 1;
        let payload = serde_json::to_vec(&Probe)
            .map(Bytes::from)
            .expect("encode Probe must succeed");
        self.upstream
            .append(
                self.upstream_key,
                vec![AppendingEvent {
                    sequence: sent,
                    event_type: Probe::EVENT_TYPE,
                    payload,
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("append Probe must succeed");

        let barrier = Instant::now() + BARRIER_TIMEOUT;
        loop {
            let reported = self.reports.lock().expect("reports mutex").clone();
            if let Some(report) = reported.get(sent as usize - 1) {
                return report.clone();
            }
            assert!(
                Instant::now() < barrier,
                "timed out waiting for probe #{sent}'s report; saw {reported:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_for_fires(&self, expected: usize) -> Vec<String> {
        let barrier = Instant::now() + BARRIER_TIMEOUT;
        loop {
            let current = self.fires_so_far();
            if current.len() >= expected {
                return current;
            }
            assert!(
                Instant::now() < barrier,
                "timed out waiting for {expected} firing(s) to reach the \
                 deadline arm; saw {current:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn fires_so_far(&self) -> Vec<String> {
        self.fires.lock().expect("fires mutex").clone()
    }
}

// Tests

/// Acceptance: a firing whose decision became durable is not fired again.
///
/// Given a saga whose pending deadline fires,
/// When the decision it was handled into is persisted,
/// Then the `fired` marker is on the stream after that decision, and a restart
/// finds the deadline neither pending nor re-armed.
#[tokio::test]
async fn a_firing_whose_decision_landed_writes_its_fired_marker_after_the_decision() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Decide,
            label: "decided".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-1a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["decided".to_owned()],
        "the seeded deadline must be re-armed by replay and delivered to the \
         deadline arm"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "recorded", "fired"]),
        "the `fired` marker settles the firing, so it may only be written once \
         the decision it settles is durable — and nothing else belongs on the \
         stream for a firing, which consumes no upstream input"
    );

    let settled = first.probe().await;
    assert!(
        settled.is_empty(),
        "a deadline whose `fired` marker is durable is no longer pending; \
         pending={settled:?}"
    );

    // Restart: a second incarnation over the same store, with a scheduler of
    // its own that holds nothing.
    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-1b").await;
    let replayed = second.probe().await;
    assert!(
        replayed.is_empty(),
        "replay must fold the durable `fired` marker out, leaving nothing to \
         re-arm; pending={replayed:?}"
    );
    tokio::time::sleep(QUIET_WINDOW).await;
    assert!(
        second.fires_so_far().is_empty(),
        "a settled deadline must not reach `handle` again; its due time is long \
         past, so a re-armed one would have fired at once"
    );
}

/// Acceptance: a firing whose decision never became durable is tried again.
///
/// Given a pending deadline that fires and a store that refuses the decision's
/// append until the interpreter gives up,
/// When the saga is restarted and replays,
/// Then the deadline is pending again, re-armed, and reaches `handle` again —
/// and no `fired` marker for it was ever on the stream.
#[tokio::test]
async fn a_firing_whose_decision_never_landed_is_re_armed_and_re_delivered_after_a_restart() {
    let refusing = Arc::new(AtomicBool::new(false));
    let saga_store: Arc<dyn EventStore> = Arc::new(RejectingStore {
        inner: InMemoryEventStore::default(),
        reject: Reject::Decision,
        armed: Arc::clone(&refusing),
    });
    let saga_id = SagaId::new(SAGA_ID);
    let anchor = seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Decide,
            label: "retried".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    // From here the saga's own decision cannot be persisted.
    refusing.store(true, Ordering::SeqCst);

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-2a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["retried".to_owned()],
        "the seeded deadline must fire before its decision can fail to land"
    );

    let still_pending = first.probe().await;
    assert_eq!(
        still_pending,
        vec![(TIMER.to_owned(), due(anchor, SEED_DELAY))],
        "the decision is in no store, so the deadline is still pending by the \
         journal's account — the only one the view follows"
    );
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled"],
        "a firing whose decision never landed must leave no `fired` marker: \
         replay folds one out of the journal, and the deadline would never be \
         re-armed again"
    );

    // Restart with the store accepting again: the unfired schedule fact is the
    // whole of what the second incarnation has to go on.
    refusing.store(false, Ordering::SeqCst);
    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-2b").await;
    assert_eq!(
        second.wait_for_fires(1).await,
        vec!["retried".to_owned()],
        "the deadline left pending must be re-armed and reach `handle` again, \
         carrying the message the schedule fact holds; its due time is past, so \
         re-arming fires it at once"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    let stream = stream_facts(&events);
    let decision = position_of(&stream, "recorded").unwrap_or_else(|| {
        panic!("the re-made decision must be on the stream; it held {stream:?}")
    });
    let settled = position_of(&stream, "fired")
        .unwrap_or_else(|| panic!("the retried firing must be settled; it held {stream:?}"));
    assert!(
        decision < settled,
        "the retry settles the same way the first attempt would have: the \
         decision first, then the marker that closes the deadline out; the \
         stream held {stream:?}"
    );
    assert_eq!(
        count_schedule_transition(&events, "fired"),
        1,
        "the deadline is settled exactly once, by the activation whose decision \
         actually landed; the stream held {stream:?}"
    );
}

/// Acceptance: a firing the saga terminated on writes no `fired` marker.
///
/// Given a deadline arm that returns `End`,
/// When the firing is handled,
/// Then the stream holds the `Ended` fact and no `fired` marker: a terminated
/// saga re-arms nothing, so there is nothing for a settlement to prevent.
#[tokio::test]
async fn a_firing_that_ends_the_saga_writes_no_fired_marker() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Finish,
            label: "finished".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    let only = Incarnation::start(&saga_store, &saga_id, "settle-up-3").await;
    assert_eq!(
        only.wait_for_fires(1).await,
        vec!["finished".to_owned()],
        "the seeded deadline must reach the arm that ends the saga"
    );
    wait_for_ended(&saga_store, &saga_id).await;

    // The terminal fact is the last thing this turn writes and the process stops
    // on it, so outlasting the turn is what makes "and nothing after it" a claim
    // rather than a snapshot.
    tokio::time::sleep(QUIET_WINDOW).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "ended"]),
        "a saga that terminated on a firing re-arms nothing, so the firing needs \
         no settlement — and a `fired` marker written here would record the \
         closing of a deadline the terminal fact already closed"
    );
}

/// Acceptance: a firing `handle` refused is settled once.
///
/// Given a deadline arm that returns `Err`,
/// When the firing is handled and the saga is then restarted,
/// Then the `fired` marker is the whole of what the firing left, the deadline is
/// not re-armed, and `handle` is not called again.
#[tokio::test]
async fn a_refused_firing_is_settled_by_its_fired_marker_alone() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Refuse,
            label: "refused".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-4a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["refused".to_owned()],
        "the seeded deadline must reach the arm that refuses it"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "fired"]),
        "a refusal produces no decision, so the `fired` marker is the whole of \
         what the firing leaves — the discipline an upstream record `handle` \
         refused is settled by, where the position alone is that row"
    );

    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-4b").await;
    let replayed = second.probe().await;
    assert!(
        replayed.is_empty(),
        "the durable `fired` marker must leave replay nothing to re-arm; \
         pending={replayed:?}"
    );
    tokio::time::sleep(QUIET_WINDOW).await;
    assert!(
        second.fires_so_far().is_empty(),
        "a refused firing is settled once; repeating it on every restart would \
         repeat the same refusal for ever"
    );
}

/// A `message_type` the current declaration disagrees with: what a schedule
/// fact written by an earlier `DeadlineMessage` impl leaves behind.
const STALE_MESSAGE_TYPE: &str = "settle-reminder-v0";

/// Acceptance: a firing nobody can read is settled once.
///
/// Given a schedule fact whose message identity no longer matches what this
/// alphabet declares,
/// When it fires and the saga is then restarted,
/// Then the `fired` marker is the whole of what the firing left, and the
/// deadline is not re-armed — the discipline an undecodable observed record is
/// skipped by.
#[tokio::test]
async fn an_unreadable_firing_is_settled_by_its_fired_marker_alone() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Decide,
            label: "unreadable".to_owned(),
        },
        STALE_MESSAGE_TYPE,
    )
    .await;

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-5a").await;
    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "fired"]),
        "a firing that cannot be read back decides nothing, so the `fired` \
         marker is the whole of what it leaves"
    );
    assert!(
        first.fires_so_far().is_empty(),
        "the schedule fact names an identity this alphabet no longer declares, \
         so the firing must never have been handed to `handle`"
    );

    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-5b").await;
    let replayed = second.probe().await;
    assert!(
        replayed.is_empty(),
        "the durable `fired` marker must leave replay nothing to re-arm; \
         pending={replayed:?}"
    );
    tokio::time::sleep(QUIET_WINDOW).await;
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_schedule_transition(&events, "fired"),
        1,
        "a firing nobody can read is settled once; a restart that re-armed it \
         would report the same unreadable fact and write another marker"
    );
    assert!(
        second.fires_so_far().is_empty(),
        "nothing readable arrived, so the second incarnation's `handle` must not \
         have run either"
    );
}

/// Acceptance: a firing that decides nothing is settled once.
///
/// Given a deadline arm that answers `SagaEffect::None`,
/// When the firing is handled and the saga is then restarted,
/// Then the `fired` marker is the only row it left, and the deadline is not
/// re-armed.
#[tokio::test]
async fn a_firing_that_decides_nothing_is_settled_by_its_fired_marker_alone() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Nothing,
            label: "quiet".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-6a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["quiet".to_owned()],
        "the seeded deadline must reach the arm that decides nothing"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "fired"]),
        "deciding nothing still has to be settled, and the `fired` marker is the \
         only row that says so — an empty batch writes nothing of its own"
    );

    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-6b").await;
    let replayed = second.probe().await;
    assert!(
        replayed.is_empty(),
        "the durable `fired` marker must leave replay nothing to re-arm; \
         pending={replayed:?}"
    );
    tokio::time::sleep(QUIET_WINDOW).await;
    assert!(
        second.fires_so_far().is_empty(),
        "a firing that decided nothing is still settled, so it must not come \
         round again"
    );
}

/// Acceptance: a settlement that never landed leaves the firing to be repeated.
///
/// Given a firing `handle` refuses and a store that refuses the `fired` marker,
/// When the saga is restarted and replays,
/// Then the deadline is still pending, reaches `handle` again, and the
/// settlement lands this time.
///
/// This is the window the change leaves open on purpose: nothing durable says
/// the deadline fired, so the firing is repeated. A repeated report is tolerated
/// where a lost deadline is not.
#[tokio::test]
async fn a_firing_whose_fired_marker_never_landed_is_re_delivered_after_a_restart() {
    let refusing = Arc::new(AtomicBool::new(false));
    let saga_store: Arc<dyn EventStore> = Arc::new(RejectingStore {
        inner: InMemoryEventStore::default(),
        reject: Reject::FiredMarker,
        armed: Arc::clone(&refusing),
    });
    let saga_id = SagaId::new(SAGA_ID);
    let anchor = seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Refuse,
            label: "unsettled".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    // From here the row that would settle the firing cannot be persisted.
    refusing.store(true, Ordering::SeqCst);

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-7a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["unsettled".to_owned()],
        "the seeded deadline must fire before its settlement can fail to land"
    );

    let still_pending = first.probe().await;
    assert_eq!(
        still_pending,
        vec![(TIMER.to_owned(), due(anchor, SEED_DELAY))],
        "the `fired` marker never landed, so the deadline is still pending: the \
         view leaves it on the marker becoming durable, not on the firing \
         arriving"
    );
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled"],
        "nothing durable says the deadline fired"
    );

    refusing.store(false, Ordering::SeqCst);
    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-7b").await;
    assert_eq!(
        second.wait_for_fires(1).await,
        vec!["unsettled".to_owned()],
        "a firing nothing durable settled must come round again — the repeated \
         report this change accepts, in place of a deadline silently lost"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled", "fired"],
        "the retried settlement lands, closing the deadline out for good"
    );
}

/// Acceptance: a reset is judged by what landed, not by what was asked for.
///
/// Given a deadline arm that re-sets the same `TimerName` and a store that
/// refuses the schedule fact's append until the interpreter gives up,
/// When the saga is restarted and replays,
/// Then no `fired` marker was written either — so the *original* deadline is
/// still pending, is re-armed, and reaches `handle` again with the message it
/// was set with.
///
/// The discriminator for judging the reset off the effect's ADT instead: a
/// same-name `ScheduleSpec` was certainly asked for here, and skipping the
/// settlement on that alone would be right by accident. What it must not do is
/// treat the deadline as replaced when nothing replaced it — which is what the
/// re-delivery below is.
#[tokio::test]
async fn a_reset_whose_schedule_fact_never_landed_leaves_the_original_deadline_to_re_fire() {
    let refusing = Arc::new(AtomicBool::new(false));
    let saga_store: Arc<dyn EventStore> = Arc::new(RejectingStore {
        inner: InMemoryEventStore::default(),
        reject: Reject::ScheduledMarker,
        armed: Arc::clone(&refusing),
    });
    let saga_id = SagaId::new(SAGA_ID);
    let anchor = seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::Reset,
            label: "original".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    // Seeded while the store still accepted one; from here no further schedule
    // fact can be persisted.
    refusing.store(true, Ordering::SeqCst);

    let first = Incarnation::start(&saga_store, &saga_id, "settle-up-8a").await;
    assert_eq!(
        first.wait_for_fires(1).await,
        vec!["original".to_owned()],
        "the seeded deadline must fire before its reset can fail to land"
    );

    let still_pending = first.probe().await;
    assert_eq!(
        still_pending,
        vec![(TIMER.to_owned(), due(anchor, SEED_DELAY))],
        "the re-set schedule fact never landed, so the deadline still pending is \
         the original one, due where the seeded fact says — not the reset that \
         was asked for"
    );
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled"],
        "the reset did not land, so it cannot stand in for the settlement \
         either: writing a `fired` marker here would close a deadline nothing \
         replaced"
    );

    refusing.store(false, Ordering::SeqCst);
    let second = Incarnation::start(&saga_store, &saga_id, "settle-up-8b").await;
    assert_eq!(
        second.wait_for_fires(1).await,
        vec!["original".to_owned()],
        "the deadline the journal still holds is what the next activation \
         re-arms, carrying the message the seeded fact holds rather than the one \
         the lost reset would have carried"
    );

    wait_for_transition(&saga_store, &saga_id, "scheduled", 2).await;
    tokio::time::sleep(QUIET_WINDOW).await;
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        schedule_transitions(&events),
        vec!["scheduled", "scheduled"],
        "the reset that did land supersedes the old deadline by name all on its \
         own, so a `fired` marker after it would remove the deadline the saga \
         has just re-armed"
    );
}

/// Given a deadline arm that sets a deadline under a *different* name,
/// When the firing is handled,
/// Then its own deadline is still settled: the `fired` marker is written after
/// the new schedule fact and names the timer that elapsed, and the newly set one
/// stays pending.
///
/// The discriminator for "skip the settlement whenever the effect scheduled
/// anything": only a same-name reset replaces the deadline that fired, so
/// skipping here would lose this firing exactly the way writing the marker too
/// early does.
#[tokio::test]
async fn a_firing_that_sets_a_differently_named_deadline_still_writes_its_fired_marker() {
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_firing(
        &saga_store,
        &saga_id,
        Reminder {
            plan: Plan::SetOther,
            label: "separate".to_owned(),
        },
        Reminder::TYPE_NAME,
    )
    .await;

    let only = Incarnation::start(&saga_store, &saga_id, "settle-up-9").await;
    assert_eq!(
        only.wait_for_fires(1).await,
        vec!["separate".to_owned()],
        "the seeded deadline must reach the arm that sets the other one"
    );

    let events = wait_for_transition(&saga_store, &saga_id, "fired", 1).await;
    assert_eq!(
        stream_facts(&events),
        facts(&["scheduled", "scheduled", "fired"]),
        "the deadline that fired is not the one that was set, so it still needs \
         its own settlement — written after what the firing decided, as every \
         settlement is"
    );
    assert_eq!(
        fired_timer_names(&events),
        vec![TIMER.to_owned()],
        "the settlement must name the deadline that elapsed; naming the \
         newly-set one would remove a deadline nothing fired"
    );

    let pending = only.probe().await;
    assert_eq!(
        names(&pending),
        vec![OTHER_TIMER],
        "the elapsed deadline leaves the view on its own `fired` marker, and the \
         deadline just set stays in it"
    );
}
