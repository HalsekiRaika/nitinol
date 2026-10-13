//! `Deadline::pending` is a view of the journal, not of intent.
//!
//! ```gherkin
//!   Scenario: the view is restored by replay
//!     Given a saga stream holding an unfired schedule fact
//!     When the saga starts and handle runs
//!     Then pending() holds that deadline's name and due time
//!
//!   Scenario: a fired deadline leaves the view
//!     Given an unfired schedule fact that then fires
//!     When handle runs afterwards
//!     Then pending() no longer holds it
//!
//!   Scenario: an append that never landed leaves the view alone
//!     Given a store that rejects the schedule fact's append
//!     When handle sets a deadline and then runs again
//!     Then pending() holds nothing
//! ```
//!
//! ## Why the view must come from the journal
//!
//! The pending deadlines are a projection of the saga's own stream: a fact that
//! landed is pending, a fire or a cancel removes it.  A view fed by intent
//! instead — updated when `set` is *called* rather than when its append lands —
//! would disagree with the stream exactly when it matters, after a crash
//! between the two.  The third scenario is that disagreement, made observable.
//!
//! ## API contract assumed by this file
//!
//! - `Deadline::pending() -> Vec<PendingDeadline>` where a `PendingDeadline`
//!   names the timer and the instant it is due:
//!   `PendingDeadline { name: TimerName, due_at: jiff::Timestamp }`, with
//!   `due_at` the schedule fact's wall-clock anchor plus its delay.
//! - `DeadlineMessage`, `SagaComponents::deadline`, `Deadline::set`, and
//!   `#[subscribe(deadline)]` — see `saga_deadline_scheduler.rs`.
//!
//! If the view's element is shaped differently, only the projection in
//! [`names_and_due`] needs reconciling — the asserted behaviour is the
//! requirement.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_deadline_pending_view`.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_consumed_fact, encode_schedule_scheduled, CONSUMED_MARKER, SCHEDULE_MARKER};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, PendingDeadline, Saga, SagaContext, SagaEffect, SagaId,
    SagaManagerProps, Subscription, TimerName,
};

// Domain

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

/// Asks the saga to report its pending deadlines, optionally setting one first.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Probe {
    set: Option<ProbeSet>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProbeSet {
    name: String,
    after_millis: u64,
}

impl Event for Probe {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_view"), TypeName::new("Probe"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_view"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    label: String,
}

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.label.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder {
            label: String::from_utf8_lossy(payload).into_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

#[derive(SagaSubscribe)]
enum ViewInput {
    Probe(Probe),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

const SAGA_ID: &str = "deadline-view-saga";

/// What one probe reported: the view as `(name, due_at)` pairs.
type Reports = Arc<Mutex<Vec<Vec<(String, jiff::Timestamp)>>>>;
type Fires = Arc<Mutex<Vec<String>>>;

/// The projection this file asserts against.  Isolated so a differently-shaped
/// view element is one edit away from reconciled.
fn names_and_due(pending: &[PendingDeadline]) -> Vec<(String, jiff::Timestamp)> {
    let mut projected: Vec<(String, jiff::Timestamp)> = pending
        .iter()
        .map(|entry| (entry.name.as_str().to_owned(), entry.due_at))
        .collect();
    projected.sort();
    projected
}

struct ViewSaga {
    deadline: Deadline<Reminder>,
    reports: Reports,
    fires: Fires,
}

#[async_trait]
impl Saga for ViewSaga {
    type Subscribe = ViewInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ViewInput::Probe(_) => Some(SagaId::new(SAGA_ID)),
            ViewInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ViewInput::Probe(probe) => {
                self.reports
                    .lock()
                    .expect("reports mutex")
                    .push(names_and_due(&self.deadline.pending()));
                match probe.set {
                    Some(set) => Ok(self.deadline.set(
                        TimerName::new(set.name.clone()),
                        Duration::from_millis(set.after_millis),
                        Reminder { label: set.name },
                    )),
                    None => Ok(SagaEffect::None),
                }
            }
            ViewInput::Reminder(reminder) => {
                self.fires
                    .lock()
                    .expect("fires mutex")
                    .push(reminder.label.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.label),
                }))
            }
        }
    }
}

// Stores

/// Rejects any append carrying a schedule marker while `failing` is set, and
/// accepts everything else.
///
/// Models a crash before the schedule fact lands: the dead letter reporting the
/// failure still has to be writable, or the saga would hold the upstream record
/// instead of moving on and the later probe would never arrive.
struct FailScheduleAppendStore {
    inner: InMemoryEventStore,
    failing: Arc<AtomicBool>,
}

#[async_trait]
impl EventStore for FailScheduleAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.failing.load(Ordering::SeqCst)
            && events
                .iter()
                .any(|event| event.event_type == SCHEDULE_MARKER)
        {
            return Err(AppendError::Backend(
                "injected: the schedule fact never landed".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Helpers

const UPSTREAM: &str = "deadline-view-upstream";

async fn append_probe(store: &Arc<dyn EventStore>, sequence: u64, set: Option<(&str, u64)>) {
    let probe = Probe {
        set: set.map(|(name, after_millis)| ProbeSet {
            name: name.to_owned(),
            after_millis,
        }),
    };
    store
        .append(
            UPSTREAM,
            vec![AppendingEvent {
                sequence,
                event_type: Probe::EVENT_TYPE,
                payload: serde_json::to_vec(&probe)
                    .map(Bytes::from)
                    .expect("encode Probe must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Probe must succeed");
}

async fn seed_scheduled(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: u64,
    name: &str,
    after: Duration,
    anchor_unix_millis: i64,
) {
    let payload = <ReminderCodec as Codec<Reminder>>::encode(&Reminder {
        label: name.to_owned(),
    })
    .expect("encoding a Reminder must succeed");
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: SCHEDULE_MARKER,
                payload: encode_schedule_scheduled(
                    saga_id,
                    &TimerName::new(name),
                    after,
                    Reminder::TYPE_NAME,
                    Reminder::CODEC_NAME,
                    &payload,
                    anchor_unix_millis,
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a schedule fact must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_reports(
    reports: &Reports,
    expected: usize,
) -> Vec<Vec<(String, jiff::Timestamp)>> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = reports.lock().expect("reports mutex").clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} probe reports; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_fires(fires: &Fires, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = fires.lock().expect("fires mutex").clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} firings; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn spawn_view_saga(
    ps: &ProcessSystem,
    saga_store: &Arc<dyn EventStore>,
    upstream_store: &Arc<dyn EventStore>,
    reports: &Reports,
    fires: &Fires,
) -> nitinol_saga::SagaManagerProxy<ViewSaga> {
    let reports = Arc::clone(reports);
    let fires = Arc::clone(fires);
    SagaManagerProps::<ViewSaga>::new(Arc::clone(saga_store), move |components| ViewSaga {
        deadline: components.deadline(),
        reports: Arc::clone(&reports),
        fires: Arc::clone(&fires),
    })
    .observing::<Probe>(Subscription::stream(upstream_store, UPSTREAM))
    .spawn(ps)
    .await
}

/// Pair an upstream probe with a position already naming it, so its arrival
/// wakes an instance without being reported.
///
/// A host spawns an instance when a record that instance's correlation claims
/// arrives, so a test whose subject is what replay restored still needs one
/// record to arrive.  Its position is already on the stream, so it is dropped
/// before `Saga::handle` — which is what keeps the reports below counting only
/// the probes the test asks about.
async fn seed_an_inert_boot_probe(
    saga_store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    saga_sequence: u64,
    upstream_store: &Arc<dyn EventStore>,
) {
    saga_store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: saga_sequence,
                event_type: CONSUMED_MARKER,
                payload: encode_consumed_fact(UPSTREAM, 1, &Probe::EVENT_TYPE.to_string()),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a consumed position must succeed");

    append_probe(upstream_store, 1, None).await;
}

fn due(anchor_unix_millis: i64, after: Duration) -> jiff::Timestamp {
    let millis = anchor_unix_millis
        + i64::try_from(after.as_millis()).expect("the test's delays fit in an i64");
    jiff::Timestamp::from_millisecond(millis).expect("the computed due time is representable")
}

// Tests

/// Given a saga stream holding an unfired schedule fact,
/// When the saga starts and `handle` runs,
/// Then `pending()` holds that deadline's name and the instant it is due —
/// restored from the journal, with no prior incarnation in memory.
#[tokio::test]
async fn replay_restores_the_pending_deadline_view_from_the_journal() {
    let ps = ProcessSystem::new().await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let anchor = jiff::Timestamp::now().as_millisecond();
    let after = Duration::from_secs(10);
    seed_scheduled(&saga_store, &saga_id, 1, "far-off", after, anchor).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_probe(&upstream_store, 1, None).await;

    let reports: Reports = Arc::new(Mutex::new(Vec::new()));
    let fires: Fires = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_view_saga(&ps, &saga_store, &upstream_store, &reports, &fires).await;

    let observed = wait_for_reports(&reports, 1).await;

    assert_eq!(
        observed,
        vec![vec![("far-off".to_owned(), due(anchor, after))]],
        "the view must be the journal's: one pending deadline, due at the \
         schedule fact's anchor plus its delay"
    );
}

/// Given an unfired schedule fact that then fires,
/// When `handle` runs afterwards,
/// Then `pending()` no longer holds it.
#[tokio::test]
async fn a_fired_deadline_leaves_the_pending_view() {
    let ps = ProcessSystem::new().await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let anchor = jiff::Timestamp::now().as_millisecond();
    seed_scheduled(
        &saga_store,
        &saga_id,
        1,
        "soon",
        Duration::from_millis(200),
        anchor,
    )
    .await;

    // One already-consumed probe, so the instance this host fronts exists and
    // replays; it is dropped before `handle`, so the only report below is the
    // one the test asks for.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    seed_an_inert_boot_probe(&saga_store, &saga_id, 2, &upstream_store).await;
    let reports: Reports = Arc::new(Mutex::new(Vec::new()));
    let fires: Fires = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_view_saga(&ps, &saga_store, &upstream_store, &reports, &fires).await;

    // The firing is the barrier for the probe that follows it.
    assert_eq!(
        wait_for_fires(&fires, 1).await,
        vec!["soon".to_owned()],
        "the re-armed deadline must fire before the view is probed"
    );

    append_probe(&upstream_store, 2, None).await;
    let observed = wait_for_reports(&reports, 1).await;

    assert_eq!(
        observed,
        vec![Vec::new()],
        "a deadline whose `fired` marker is on the stream is no longer pending"
    );
}

/// Given a store that rejects the schedule fact's append,
/// When `handle` sets a deadline and then runs again,
/// Then `pending()` holds nothing: the view follows what landed, not what was
/// asked for.
#[tokio::test]
async fn an_append_that_never_landed_leaves_the_pending_view_empty() {
    let ps = ProcessSystem::new().await;

    let saga_store: Arc<dyn EventStore> = Arc::new(FailScheduleAppendStore {
        inner: InMemoryEventStore::default(),
        failing: Arc::new(AtomicBool::new(true)),
    });

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // The first probe sets a deadline whose fact cannot land; the second reads
    // the view back.
    append_probe(&upstream_store, 1, Some(("never-landed", 10_000))).await;

    let reports: Reports = Arc::new(Mutex::new(Vec::new()));
    let fires: Fires = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_view_saga(&ps, &saga_store, &upstream_store, &reports, &fires).await;

    wait_for_reports(&reports, 1).await;
    append_probe(&upstream_store, 2, None).await;
    let observed = wait_for_reports(&reports, 2).await;

    assert_eq!(
        observed,
        vec![Vec::new(), Vec::new()],
        "the schedule fact never landed, so nothing may be pending; a view fed \
         by intent instead of by the journal would report `never-landed` here"
    );
}
