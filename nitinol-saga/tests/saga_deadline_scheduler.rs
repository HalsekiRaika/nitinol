//! End-to-end behaviour of the DeadlineScheduler.
//!
//! These integration tests exercise the full data flow across modules:
//! `SagaEffect::schedule` / `cancel_schedule` → interpreter → `SchedulerProcess`
//! timer → `Saga::on_scheduled` dispatch → `SagaPersisted::Schedule` events on
//! the saga's own `EventStore` stream.
//!
//! ## API contract assumed by these tests
//!
//! - `Saga::ScheduledMessage` associated type; `on_scheduled(msg:
//!   Self::ScheduledMessage, ctx)`.
//! - `SagaEffect::schedule(TimerName, Duration, msg)` and
//!   `SagaEffect::cancel_schedule(TimerName)`.
//! - `nitinol_saga::spawn_scheduler(&ProcessSystem) -> SchedulerProxy`
//!   (system-wide resident scheduler) and
//!   `SagaProps::with_scheduler(SchedulerProxy)` to inject it.
//! - Schedule events are appended to the saga stream with an `EventType` whose
//!   type key is `nitinol.saga.schedule` and whose per-arm variant is
//!   `scheduled` / `cancelled` / `fired`.
//!
//! If a name differs in the implementation, only the wiring lines below need to
//! be reconciled — the asserted behaviour is the requirement.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TimerName};

// Local JSON codec — self-contained so this file does not couple to the shared
// `common` module.
use nitinol_eventsource::codec::Codec;

#[derive(Default)]
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
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_it"), TypeName::new("Trigger"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_it"), TypeName::new("Recorded"));
}

/// The saga's `Self::ScheduledMessage` — a typed payload that must survive the
/// serialize → persist → deserialize → `on_scheduled` round trip.
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

    fn apply(&mut self, _event: Recorded) {}

    async fn handle(
        &mut self,
        event: Trigger,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        if event.cancel {
            return Ok(SagaEffect::cancel_schedule(TimerName::new("reminder")));
        }
        Ok(SagaEffect::schedule(
            TimerName::new("reminder"),
            Duration::from_millis(event.after_millis),
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
            .expect("fired mutex")
            .push(message.note.clone());
        Ok(SagaEffect::persist(Recorded {
            label: format!("fired:{}", message.note),
        }))
    }
}

// Helpers

/// Type-level identity of a schedule event: family `nitinol.saga`,
/// type name `schedule`.  Per-arm variants (`scheduled` / `cancelled` /
/// `fired`) share this type key.
const SCHEDULE_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("schedule"));

fn count_schedule_variant(events: &[LoadedEvent], variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == SCHEDULE_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
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

// Tests

/// Given a saga that schedules a `Reminder` after a short delay,
/// When the timer elapses,
/// Then `on_scheduled` is invoked with the deserialised typed payload.
#[tokio::test]
async fn schedule_fires_and_delivers_typed_message() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-1",
        1,
        Trigger {
            note: "hello".to_owned(),
            after_millis: 150,
            cancel: false,
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-fires-1");
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let fired_producer = Arc::clone(&fired);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        TimerSaga {
            fired: Arc::clone(&fired_producer),
        }
    })
    .with_codec(system.codec::<Recorded>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Trigger>(),
        SequenceCursor::Stream {
            key: "deadline-upstream-1".to_owned(),
            after: 0,
        },
        move |_event: &Trigger| Some(routed.clone()),
    )
    .with_scheduler(scheduler.clone())
    .spawn(system.process_system())
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert!(
        ok,
        "the scheduled reminder must fire within 5s; fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["hello".to_owned()],
        "on_scheduled must receive exactly the scheduled typed payload"
    );
}

/// Given a schedule and its subsequent firing,
/// Then the saga's own stream carries a `scheduled` marker (on schedule) and a
/// `fired` marker (on dispatch), both under the `nitinol.saga.schedule` type
/// key.
#[tokio::test]
async fn schedule_and_fire_persist_schedule_markers_on_saga_stream() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-2",
        1,
        Trigger {
            note: "mark".to_owned(),
            after_millis: 150,
            cancel: false,
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-markers-1");
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let fired_producer = Arc::clone(&fired);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        TimerSaga {
            fired: Arc::clone(&fired_producer),
        }
    })
    .with_codec(system.codec::<Recorded>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Trigger>(),
        SequenceCursor::Stream {
            key: "deadline-upstream-2".to_owned(),
            after: 0,
        },
        move |_event: &Trigger| Some(routed.clone()),
    )
    .with_scheduler(scheduler.clone())
    .spawn(system.process_system())
    .await;

    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;
    assert!(ok, "the reminder must fire so a `fired` marker is written");

    // Allow the post-fire persist to settle.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_schedule_variant(&events, "scheduled"),
        1,
        "scheduling must append exactly one `nitinol.saga.schedule.scheduled` marker"
    );
    assert_eq!(
        count_schedule_variant(&events, "fired"),
        1,
        "firing must append exactly one `nitinol.saga.schedule.fired` marker"
    );
    assert_eq!(
        count_schedule_variant(&events, "cancelled"),
        0,
        "no cancellation occurred, so no `cancelled` marker may be written"
    );
}

/// `startSingleTimer` semantics: re-scheduling the same `TimerName`
/// auto-cancels the earlier registration, so only the latest schedule fires.
#[tokio::test]
async fn rescheduling_same_name_auto_cancels_the_earlier_timer() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // First: a long delay under name "reminder".
    append_trigger(
        &upstream_store,
        "deadline-upstream-3",
        1,
        Trigger {
            note: "stale".to_owned(),
            after_millis: 1_500,
            cancel: false,
        },
    )
    .await;
    // Second: a short delay under the SAME name — must replace the first.
    append_trigger(
        &upstream_store,
        "deadline-upstream-3",
        2,
        Trigger {
            note: "fresh".to_owned(),
            after_millis: 150,
            cancel: false,
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-reschedule-1");
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let fired_producer = Arc::clone(&fired);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        TimerSaga {
            fired: Arc::clone(&fired_producer),
        }
    })
    .with_codec(system.codec::<Recorded>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Trigger>(),
        SequenceCursor::Stream {
            key: "deadline-upstream-3".to_owned(),
            after: 0,
        },
        move |_event: &Trigger| Some(routed.clone()),
    )
    .with_scheduler(scheduler.clone())
    .spawn(system.process_system())
    .await;

    // Wait for the fresh timer to fire.
    let fired_check = Arc::clone(&fired);
    let ok = wait_until(Duration::from_secs(5), || {
        !fired_check.lock().expect("fired mutex").is_empty()
    })
    .await;
    assert!(ok, "the re-scheduled (fresh) timer must fire");

    // Wait past the stale timer's original deadline to prove it was cancelled.
    tokio::time::sleep(Duration::from_millis(1_800)).await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert_eq!(
        seen,
        vec!["fresh".to_owned()],
        "only the latest schedule under a re-used name may fire (the stale one is auto-cancelled)"
    );
}

/// `cancel_schedule(name)` cancels a pending timer so it never fires, and
/// records a `cancelled` marker on the saga stream.
#[tokio::test]
async fn cancel_schedule_prevents_the_timer_from_firing() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_store,
        "deadline-upstream-4",
        1,
        Trigger {
            note: "doomed".to_owned(),
            after_millis: 600,
            cancel: false,
        },
    )
    .await;
    append_trigger(
        &upstream_store,
        "deadline-upstream-4",
        2,
        Trigger {
            note: String::new(),
            after_millis: 0,
            cancel: true,
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-cancel-1");
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let fired_producer = Arc::clone(&fired);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        TimerSaga {
            fired: Arc::clone(&fired_producer),
        }
    })
    .with_codec(system.codec::<Recorded>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Trigger>(),
        SequenceCursor::Stream {
            key: "deadline-upstream-4".to_owned(),
            after: 0,
        },
        move |_event: &Trigger| Some(routed.clone()),
    )
    .with_scheduler(scheduler.clone())
    .spawn(system.process_system())
    .await;

    // Wait past the original deadline; the cancel must have won.
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    let seen = fired.lock().expect("fired mutex").clone();
    assert!(
        seen.is_empty(),
        "a cancelled schedule must never fire; fired={seen:?}"
    );

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_schedule_variant(&events, "cancelled"),
        1,
        "cancel_schedule must append exactly one `nitinol.saga.schedule.cancelled` marker"
    );
    assert_eq!(
        count_schedule_variant(&events, "fired"),
        0,
        "a cancelled schedule must not produce a `fired` marker"
    );
}

/// A fresh saga incarnation started against a store that already holds an
/// unfired `scheduled` marker must re-register the timer from replay and fire
/// it — even though this incarnation never receives the upstream trigger itself.
#[tokio::test]
async fn replay_reregisters_unfired_schedule_and_fires_it() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-replay-1");

    // Incarnation #1 receives the trigger and persists a `scheduled` marker with
    // a delay long enough that it has not yet fired when #2 starts.
    let upstream_a: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_trigger(
        &upstream_a,
        "deadline-upstream-5a",
        1,
        Trigger {
            note: "survivor".to_owned(),
            after_millis: 2_000,
            cancel: false,
        },
    )
    .await;

    let fired_1 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fired_1_producer = Arc::clone(&fired_1);
    let routed_1 = saga_id.clone();
    let _proxy_1 =
        SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TimerSaga {
                fired: Arc::clone(&fired_1_producer),
            }
        })
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            Arc::clone(&upstream_a),
            system.codec::<Trigger>(),
            SequenceCursor::Stream {
                key: "deadline-upstream-5a".to_owned(),
                after: 0,
            },
            move |_event: &Trigger| Some(routed_1.clone()),
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    // Wait until the `scheduled` marker is durably on the saga stream.
    let store_for_wait = Arc::clone(&saga_store);
    let id_for_wait = saga_id.clone();
    let mut scheduled_seen = false;
    for _ in 0..200 {
        let events = load_saga_events(&store_for_wait, &id_for_wait).await;
        if count_schedule_variant(&events, "scheduled") >= 1 {
            scheduled_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        scheduled_seen,
        "incarnation #1 must persist a `scheduled` marker"
    );

    // Incarnation #2 shares the same saga id + store + scheduler but subscribes
    // to an EMPTY upstream, so it can only learn about the schedule via replay.
    let upstream_b: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let fired_2 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fired_2_producer = Arc::clone(&fired_2);
    let routed_2 = saga_id.clone();
    let _proxy_2 =
        SagaProps::<TimerSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            TimerSaga {
                fired: Arc::clone(&fired_2_producer),
            }
        })
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            Arc::clone(&upstream_b),
            system.codec::<Trigger>(),
            SequenceCursor::Stream {
                key: "deadline-upstream-5b-empty".to_owned(),
                after: 0,
            },
            move |_event: &Trigger| Some(routed_2.clone()),
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    // Incarnation #2 must fire the re-registered timer even though it never saw
    // the upstream trigger.
    let fired_2_check = Arc::clone(&fired_2);
    let ok = wait_until(Duration::from_secs(8), || {
        !fired_2_check.lock().expect("fired mutex").is_empty()
    })
    .await;

    let seen = fired_2.lock().expect("fired mutex").clone();
    assert!(
        ok,
        "the replaying incarnation must re-register and fire the unfired schedule; fired={seen:?}"
    );
    assert_eq!(
        seen,
        vec!["survivor".to_owned()],
        "the re-registered timer must carry the original scheduled payload"
    );
}

// Regression: Fired marker ordering when on_scheduled re-schedules

/// Typed payload for the marker-ordering regression saga.  `generation` controls
/// whether `on_scheduled` re-schedules: generation 0 triggers a re-schedule
/// under the same name with generation 1; generation 1 is a no-op.  This
/// keeps the behaviour identical regardless of which incarnation handles the
/// timer so the test never loops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct GenerationReminder {
    note: String,
    generation: u32,
}

/// A saga whose `on_scheduled` re-schedules under the **same** `TimerName`
/// exactly once (controlled by `generation` in the payload, not by a
/// per-instance counter, so the semantics are incarnation-independent).
///
/// This exercises the marker ordering rule: `Fired` must be persisted before
/// the new `Scheduled` so that replay folds the stream as:
///   insert(name) → remove(name) → insert(name)  ⟹  active
/// rather than:
///   insert(name) → insert(name) → remove(name)  ⟹  gone  (broken ordering)
struct OnceRescheduleSaga {
    fires: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for OnceRescheduleSaga {
    type SubscribedEvent = Trigger;
    type Event = Recorded;
    type Error = std::convert::Infallible;
    type ScheduledMessage = GenerationReminder;

    fn apply(&mut self, _event: Recorded) {}

    async fn handle(
        &mut self,
        event: Trigger,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        Ok(SagaEffect::schedule(
            TimerName::new("repeat"),
            Duration::from_millis(event.after_millis),
            GenerationReminder {
                note: event.note,
                generation: 0,
            },
        ))
    }

    async fn on_scheduled(
        &mut self,
        message: GenerationReminder,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Recorded>, Self::Error> {
        self.fires
            .lock()
            .expect("fires mutex")
            .push(message.note.clone());
        // Re-schedule under the same name only for generation 0.
        if message.generation == 0 {
            return Ok(SagaEffect::schedule(
                TimerName::new("repeat"),
                Duration::from_millis(300),
                GenerationReminder {
                    note: format!("{}-2nd", message.note),
                    generation: 1,
                },
            ));
        }
        Ok(SagaEffect::empty())
    }
}

/// Regression: when `on_scheduled` re-schedules under the same
/// `TimerName`, the second incarnation (replay) must still see the
/// re-scheduled timer as active and eventually fire it.
///
/// Without the fix, the event stream order is:
///   Scheduled → (new) Scheduled → Fired
/// and replay's `Fired` handler removes the new schedule by name, leaving it
/// invisible to the second incarnation.
///
/// With the fix, the order is:
///   Scheduled → Fired → (new) Scheduled
/// and replay correctly leaves the new schedule active for re-registration.
#[tokio::test]
async fn on_scheduled_reschedule_same_name_survives_replay() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let scheduler = nitinol_saga::spawn_scheduler(system.process_system()).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // A single trigger that starts the first "repeat" timer.
    append_trigger(
        &upstream_store,
        "deadline-upstream-arch004",
        1,
        Trigger {
            note: "first".to_owned(),
            after_millis: 150,
            cancel: false,
        },
    )
    .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("deadline-arch004-1");

    // Incarnation #1: receives trigger, fires first timer, re-schedules.
    let fires_1 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fires_1_producer = Arc::clone(&fires_1);
    let routed_1 = saga_id.clone();
    let _proxy_1 =
        SagaProps::<OnceRescheduleSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            OnceRescheduleSaga {
                fires: Arc::clone(&fires_1_producer),
            }
        })
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Trigger>(),
            SequenceCursor::Stream {
                key: "deadline-upstream-arch004".to_owned(),
                after: 0,
            },
            move |_event: &Trigger| Some(routed_1.clone()),
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    // Wait for incarnation #1 to fire the first timer and persist the
    // re-scheduled "repeat" timer.
    let fires_1_check = Arc::clone(&fires_1);
    let ok = wait_until(Duration::from_secs(5), || {
        !fires_1_check.lock().expect("fires mutex").is_empty()
    })
    .await;
    assert!(ok, "incarnation #1 must fire the first timer");

    // Allow the Fired marker and new Scheduled marker to be written.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify stream has: 1 scheduled (initial), 1 fired, 1 scheduled (re-schedule).
    // The key invariant: Fired appears before the second Scheduled in the stream.
    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_schedule_variant(&events, "scheduled"),
        2,
        "must have exactly 2 scheduled markers (initial + re-schedule)"
    );
    assert_eq!(
        count_schedule_variant(&events, "fired"),
        1,
        "first fire must produce exactly 1 fired marker"
    );

    // Verify event ordering: Fired must come BEFORE the second Scheduled.
    let schedule_positions: Vec<(usize, &str)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.event_type.type_key() == SCHEDULE_TYPE.type_key())
        .map(|(i, e)| {
            let variant = e.event_type.variant().map(|v| v.as_str()).unwrap_or("");
            (i, variant)
        })
        .collect();
    // positions should be: (_, "scheduled"), (_, "fired"), (_, "scheduled")
    assert_eq!(
        schedule_positions.len(),
        3,
        "must have 3 schedule-family events"
    );
    assert_eq!(
        schedule_positions[0].1, "scheduled",
        "first schedule event must be 'scheduled'"
    );
    assert_eq!(
        schedule_positions[1].1, "fired",
        "Fired must precede the re-scheduled Scheduled"
    );
    assert_eq!(
        schedule_positions[2].1, "scheduled",
        "re-scheduled event must follow Fired"
    );

    // Incarnation #2: shares the same store but subscribes to an empty
    // upstream, so it can only learn about the re-schedule via replay.
    // It must re-register and eventually fire the second timer.
    let upstream_empty: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let fires_2 = Arc::new(Mutex::new(Vec::<String>::new()));
    let fires_2_producer = Arc::clone(&fires_2);
    let routed_2 = saga_id.clone();
    let _proxy_2 =
        SagaProps::<OnceRescheduleSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            OnceRescheduleSaga {
                fires: Arc::clone(&fires_2_producer),
            }
        })
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            Arc::clone(&upstream_empty),
            system.codec::<Trigger>(),
            SequenceCursor::Stream {
                key: "deadline-upstream-arch004-empty".to_owned(),
                after: 0,
            },
            move |_event: &Trigger| Some(routed_2.clone()),
        )
        .with_scheduler(scheduler.clone())
        .spawn(system.process_system())
        .await;

    // Incarnation #2 must fire the re-scheduled timer (the "first-2nd" one).
    let fires_2_check = Arc::clone(&fires_2);
    let ok = wait_until(Duration::from_secs(8), || {
        !fires_2_check.lock().expect("fires mutex").is_empty()
    })
    .await;

    let seen_2 = fires_2.lock().expect("fires mutex").clone();
    assert!(
        ok,
        "incarnation #2 must re-register and fire the re-scheduled timer after replay; fired={seen_2:?}"
    );
    assert_eq!(
        seen_2,
        vec!["first-2nd".to_owned()],
        "the re-scheduled timer must carry the payload set by on_scheduled (not the original)"
    );
}
