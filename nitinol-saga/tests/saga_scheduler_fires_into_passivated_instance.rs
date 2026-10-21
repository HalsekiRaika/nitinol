//! A deadline outlives the instance that set it.
//!
//! Passivation and deadlines cross here: an instance that schedules a timer
//! longer than its idle window is gone by the time the timer elapses.  The
//! firing therefore cannot be addressed to that instance — it is addressed to
//! the manager, which is the only thing that can bring the instance back, and
//! which restores it before delivering.
//!
//! Three ways that can be got wrong are all observable below:
//! - the firing is addressed to the instance's own proxy → the `tell` lands in
//!   a dead process and the deadline arm never runs;
//! - the instance's stop releases its timers unconditionally → the deadline is
//!   gone from the scheduler before it can elapse, and nothing fires at all;
//! - the firing is dropped when the addressee is not resident → same silence,
//!   for a third reason.
//!
//! Every one of them leaves the fire-wait below timing out, and each is
//! distinguished from "the instance simply stayed resident" by the producer
//! count: a revived instance is a second lifecycle.
//!
//! Delivery is at-least-once (`nitinol_saga` crate docs): the revived
//! incarnation also replays the still-unfired `scheduled` marker and re-arms it
//! as already due, so the firing may be observed more than once.  The
//! assertions therefore bound the payload, not the count.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_scheduler_fires_into_passivated_instance`.

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

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

/// The one instance every `Ping` correlates to.
const INSTANCE_ID: &str = "sched-passivation-instance";
const UPSTREAM_KEY: &str = "sched-passivation-pings";

/// Short enough that the instance is certainly gone well before the deadline,
/// long enough that it cannot elapse between the manager spawning the instance
/// and delivering the event that caused the spawn — same reasoning as
/// `saga_manager_passivation.rs`.
const PASSIVATE_AFTER: Duration = Duration::from_millis(400);

/// The scheduled delay: several passivation windows, so the instance that set
/// it has been stopped for most of the wait by the time it elapses.
const DEADLINE: Duration = Duration::from_millis(2_500);

/// What the reminder carries, so a firing delivered to the wrong incarnation or
/// stripped of its payload is distinguishable from the real thing.
const NOTE: &str = "survives-passivation";

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    note: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("sched.passivation"), TypeName::new("Ping"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded;

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("sched.passivation"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// What the deadline carries.  It names the `Codec` that writes it into the
/// schedule fact, which is what the firing is read back through after the
/// instance that set it is gone.
#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    note: String,
}

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

/// The whole alphabet this saga reacts to: one observed source, and its own
/// time — the two that cross in this file.
#[derive(SagaSubscribe)]
enum ReminderInput {
    Ping(Ping),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

// Saga under test

/// What the instance did, in the order it did it.
///
/// One ordered log rather than two counters: the firing has to be observable as
/// something that happened *after* the schedule, and a timeout has to be able
/// to report how far the run actually got.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Scheduled,
    Fired { note: String },
}

struct ReminderSaga {
    deadline: Deadline<Reminder>,
    steps: Arc<Mutex<Vec<Step>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for ReminderSaga {
    type Subscribe = ReminderInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ReminderInput::Ping(_) => Some(SagaId::new(INSTANCE_ID)),
            // A firing is addressed to the instance that set it — here, through
            // the manager that restores it — and never travels the subscription
            // path correlation answers for.
            ReminderInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ReminderInput::Ping(ping) => {
                self.record(Step::Scheduled);
                Ok(self.deadline.set(
                    TimerName::new("reminder"),
                    DEADLINE,
                    Reminder { note: ping.note },
                ))
            }
            ReminderInput::Reminder(reminder) => {
                self.record(Step::Fired {
                    note: reminder.note,
                });
                Ok(SagaEffect::persist(Recorded))
            }
        }
    }
}

impl ReminderSaga {
    fn record(&self, step: Step) {
        self.steps
            .lock()
            .expect("steps mutex is never poisoned: no holder panics while the guard is alive")
            .push(step);
        self.notify.notify_one();
    }
}

// Helpers

async fn append_ping(store: &Journal, sequence: u64, note: &str) {
    let payload = serde_json::to_vec(&Ping {
        note: note.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode Ping must succeed");
    store
        .append(
            UPSTREAM_KEY,
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

fn steps_snapshot(steps: &Arc<Mutex<Vec<Step>>>) -> Vec<Step> {
    steps
        .lock()
        .expect("steps mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

/// Wait until `done` holds of the recorded steps.  `context` states what the
/// wait proves, so a timeout reports the broken contract rather than only the
/// predicate that never held.
async fn wait_for_steps(
    steps: &Arc<Mutex<Vec<Step>>>,
    notify: &Arc<Notify>,
    budget: Duration,
    done: impl Fn(&[Step]) -> bool,
    context: &str,
) {
    tokio::time::timeout(budget, async {
        loop {
            let notified = notify.notified();
            if done(&steps_snapshot(steps)) {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out after {budget:?} — {context} (recorded {:?})",
            steps_snapshot(steps)
        )
    });
}

fn fired_notes(steps: &[Step]) -> Vec<&str> {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Fired { note } => Some(note.as_str()),
            Step::Scheduled => None,
        })
        .collect()
}

// Test

/// Given a manager that passivates idle instances and an instance that
/// scheduled a deadline longer than its passivation window,
/// when the deadline elapses,
/// then the manager spawns the instance again and the firing reaches it with
/// the payload that was scheduled.
#[tokio::test]
async fn scheduler_firing_activates_a_passivated_instance() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    // One journal: the ping stream the saga observes and the instance's own
    // stream are tenants of the store the system distributes.
    let journal = system.persistence().event_store();

    let steps: Arc<Mutex<Vec<Step>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    // Counted in the producer, so it counts instance lifecycles actually built
    // rather than deliveries.
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let steps_for_producer = Arc::clone(&steps);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _host = SagaManagerProps::<ReminderSaga>::new(move |components| {
        spawned_for_producer.fetch_add(1, Ordering::SeqCst);
        ReminderSaga {
            deadline: components.deadline(),
            steps: Arc::clone(&steps_for_producer),
            notify: Arc::clone(&notify_for_producer),
        }
    })
    .observing::<Ping>(Subscription::stream(UPSTREAM_KEY))
    .with_instance_passivation(PASSIVATE_AFTER)
    .spawn(&system)
    .await;

    append_ping(&journal, 1, NOTE).await;
    wait_for_steps(
        &steps,
        &notify,
        Duration::from_secs(5),
        |steps| steps.contains(&Step::Scheduled),
        "the manager must deliver the ping to the instance it spawns, so the \
         instance can register its deadline",
    )
    .await;
    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        1,
        "exactly one instance may have been built before the deadline elapses"
    );

    // When: the passivation window elapsing while the deadline is still pending
    // is the condition under test, so the test waits it out rather than
    // synchronising around it.  The multiplier keeps the wait clear of the
    // runtime's idle-timer granularity, and DEADLINE keeps it clear of the
    // firing.
    tokio::time::sleep(PASSIVATE_AFTER * 3).await;
    assert_eq!(
        steps_snapshot(&steps),
        vec![Step::Scheduled],
        "the deadline must not have fired yet: the rest of this test observes \
         what happens to a deadline whose owner is already gone"
    );

    wait_for_steps(
        &steps,
        &notify,
        DEADLINE * 4,
        |steps| !fired_notes(steps).is_empty(),
        "a deadline whose instance was passivated must still fire and reach \
         that instance through the manager",
    )
    .await;

    // Then
    let recorded = steps_snapshot(&steps);
    assert!(
        instances_spawned.load(Ordering::SeqCst) >= 2,
        "the firing must have been delivered through an instance the manager \
         restored: a single lifecycle means the instance never passivated, so \
         this run never exercised the crossing (recorded {recorded:?})"
    );

    let notes = fired_notes(&recorded);
    assert!(
        notes.iter().all(|note| *note == NOTE),
        "every firing delivered to the restored instance must carry the \
         payload that was scheduled, got {notes:?}"
    );
}
