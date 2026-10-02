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
//!   a dead process and `on_scheduled` never runs;
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
use common::{resident_scheduler, JsonCodec};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, TimerName};

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

/// The saga's `Self::ScheduledMessage`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reminder {
    note: String,
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
    steps: Arc<Mutex<Vec<Step>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for ReminderSaga {
    type SubscribedEvent = Ping;
    type Event = Recorded;
    type ScheduledMessage = Reminder;
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(INSTANCE_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.record(Step::Scheduled);
        Ok(SagaEffect::schedule(
            TimerName::new("reminder"),
            DEADLINE,
            Reminder { note: event.note },
        ))
    }

    async fn on_scheduled(
        &mut self,
        message: Self::ScheduledMessage,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.record(Step::Fired { note: message.note });
        Ok(SagaEffect::persist(Recorded))
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

async fn append_ping(store: &Arc<dyn EventStore>, sequence: u64, note: &str) {
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
    let system = EventSourceSystem::builder(ps).build();
    let scheduler = resident_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let steps: Arc<Mutex<Vec<Step>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    // Counted in the producer, so it counts instance lifecycles actually built
    // rather than deliveries.
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let steps_for_producer = Arc::clone(&steps);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy =
        SagaManagerProps::<ReminderSaga>::new(Arc::clone(&saga_store), move || {
            spawned_for_producer.fetch_add(1, Ordering::SeqCst);
            ReminderSaga {
                steps: Arc::clone(&steps_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .with_subscription(
            Arc::clone(&upstream_store),
            SequenceCursor::Stream {
                key: UPSTREAM_KEY.to_owned(),
                after: 0,
            },
        )
        .with_instance_passivation(PASSIVATE_AFTER)
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    append_ping(&upstream_store, 1, NOTE).await;
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
