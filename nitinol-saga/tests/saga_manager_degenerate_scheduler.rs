//! The degenerate single-instance saga has its own time.
//!
//! `saga_manager_degenerate_single_instance.rs` pins what a saga whose
//! `Saga::correlate` is constant writes and where, but does not exercise a
//! deadline — so an instance whose schedule markers persist without ever
//! registering a timer would still pass it.  This file is the deadline
//! counterpart, mirroring
//! `saga_deadline_scheduler.rs::a_deadline_fires_into_the_deadline_arm_of_handle`
//! on a store-bound `SagaManagerProps` rather than on a hosted one.
//!
//! There is no scheduler to wire anywhere: a saga whose alphabet declares a
//! `#[subscribe(deadline)]` variant has the system's resident
//! `SchedulerProcess` resolved for it at spawn, and the manager hands that same
//! core to every instance it spawns.  What this file observes is that the
//! handing-over actually happens — that the degenerate instance's deadline is
//! armed and not merely written down.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

/// The single instance every upstream event correlates to — same shape as
/// `saga_manager_degenerate_single_instance.rs::RESIDENT_SAGA_ID`.
const RESIDENT_SAGA_ID: &str = "mgr-scheduler-degenerate-1";
const UPSTREAM_KEY: &str = "mgr-scheduler-degenerate-upstream";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Trigger {
    note: String,
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.scheduler"), TypeName::new("Trigger"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.scheduler"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// What the deadline carries — a typed payload that must survive the
/// encode -> persist -> decode -> deadline-arm round trip.  It names the
/// `Codec` that writes it into the schedule fact, as every other persisted
/// value does.
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
/// time.
#[derive(SagaSubscribe)]
enum TimerInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

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
            TimerInput::Trigger(_) => Some(SagaId::new(RESIDENT_SAGA_ID)),
            // A firing is addressed to the instance that set it and never
            // travels the subscription path.
            TimerInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        match input {
            TimerInput::Trigger(trigger) => Ok(self.deadline.set(
                TimerName::new("reminder"),
                Duration::from_millis(150),
                Reminder { note: trigger.note },
            )),
            TimerInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect(
                        "fired mutex is never poisoned: no holder panics while the guard is alive",
                    )
                    .push(reminder.note.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.note),
                }))
            }
        }
    }
}

// Helpers

async fn append_trigger(store: &Journal, sequence: u64, trigger: Trigger) {
    let payload = serde_json::to_vec(&trigger)
        .map(Bytes::from)
        .expect("encode Trigger must succeed");
    store
        .append(
            UPSTREAM_KEY,
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

// Test

/// Given a manager whose `Saga::correlate` always answers the same id (the
/// degenerate single-instance case) and whose saga declares a deadline in its
/// alphabet,
/// when the resulting instance sets that deadline,
/// then the resident scheduler the manager resolved must actually fire the
/// timer and deliver the typed payload to the deadline arm of `handle` — the
/// same outcome a saga correlating to many instances gets.  Without handing
/// the resolved core through to the manager's spawned instances, the schedule
/// marker would persist but no timer would ever fire.
#[tokio::test]
async fn manager_fires_the_degenerate_instances_timer() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        1,
        Trigger {
            note: "hello".to_owned(),
        },
    )
    .await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));
    let fired_producer = Arc::clone(&fired);

    let _manager_proxy =
        SagaManagerProps::<TimerSaga>::new(saga_store.clone(), move |components| TimerSaga {
            deadline: components.deadline(),
            fired: Arc::clone(&fired_producer),
        })
        .observing::<Trigger>(Subscription::stream(&upstream_store, UPSTREAM_KEY))
        .spawn(system.process_system())
        .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check
            .lock()
            .expect("fired mutex is never poisoned: no holder panics while the guard is alive")
            .is_empty()
    })
    .await;

    let seen = fired
        .lock()
        .expect("fired mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert!(
        ok,
        "the manager-spawned degenerate instance's deadline must fire within \
         5s with no scheduler wiring at all; fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["hello".to_owned()],
        "the deadline arm must receive exactly the typed payload that was set"
    );
}
