//! The degenerate single-instance manager reproduces
//! `SagaProps::with_scheduler`.
//!
//! `saga_manager_degenerate_single_instance.rs` pins the persist/tell/outbox
//! parity between a manager whose `Saga::correlate` is constant and a
//! standalone `SagaProps`, but does not exercise `SagaEffect::schedule` — so a
//! manager-spawned instance whose schedule markers persist without ever
//! registering a timer would still pass it.  This file is the schedule-effect
//! counterpart, mirroring
//! `saga_deadline_scheduler.rs::schedule_fires_and_delivers_typed_message` but
//! spawned through `SagaManagerProps::with_scheduler` instead of
//! `SagaProps::with_scheduler`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, TimerName};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.scheduler"), TypeName::new("Recorded"));
}

/// The saga's `Self::ScheduledMessage` — a typed payload that must survive the
/// serialize -> persist -> deserialize -> `on_scheduled` round trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reminder {
    note: String,
}

struct TimerSaga {
    fired: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for TimerSaga {
    type SubscribedEvent = Trigger;
    type Event = Recorded;
    type Error = std::convert::Infallible;
    type ScheduledMessage = Reminder;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
    }

    fn apply(&mut self, _event: Recorded) {}

    async fn handle(
        &mut self,
        event: Trigger,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        Ok(SagaEffect::schedule(
            TimerName::new("reminder"),
            Duration::from_millis(150),
            Reminder { note: event.note },
        ))
    }

    async fn on_scheduled(
        &mut self,
        message: Reminder,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        self.fired
            .lock()
            .expect("fired mutex is never poisoned: no holder panics while the guard is alive")
            .push(message.note.clone());
        Ok(SagaEffect::persist(Recorded {
            label: format!("fired:{}", message.note),
        }))
    }
}

// Helpers

async fn append_trigger(store: &Arc<dyn EventStore>, sequence: u64, trigger: Trigger) {
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

/// Given a manager configured with `SagaManagerProps::with_scheduler` whose
/// `Saga::correlate` always answers the same id (the degenerate single-instance
/// case),
/// when the resulting instance returns `SagaEffect::schedule`,
/// then the resident scheduler must actually fire the timer and dispatch the
/// typed payload to `on_scheduled` — the same outcome `SagaProps::with_scheduler`
/// gives the standalone builder.  Without wiring the scheduler through to the
/// manager's spawned instances, the schedule marker would persist but no timer
/// would ever fire.
#[tokio::test]
async fn manager_with_scheduler_fires_the_degenerate_instances_timer() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        1,
        Trigger {
            note: "hello".to_owned(),
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));
    let fired_producer = Arc::clone(&fired);

    let _manager_proxy =
        SagaManagerProps::<TimerSaga>::new(Arc::clone(&saga_store), move || TimerSaga {
            fired: Arc::clone(&fired_producer),
        })
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Trigger>(),
            SequenceCursor::Stream {
                key: UPSTREAM_KEY.to_owned(),
                after: 0,
            },
        )
        .with_scheduler(scheduler.clone())
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
        "the manager-spawned degenerate instance's scheduled timer must fire \
         within 5s when SagaManagerProps::with_scheduler is configured; fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["hello".to_owned()],
        "on_scheduled must receive exactly the scheduled typed payload"
    );
}
