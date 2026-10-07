//! A terminal `SagaEffect::End` must release a manager-fronted instance's live
//! timer, not only a passivation.
//!
//! `ScheduleDelivery::ThroughTheManager` keeps a deadline armed across a
//! passivation so the manager can restore the instance and hand it the
//! firing later — that is what
//! `saga_scheduler_fires_into_passivated_instance.rs` pins down.  The same
//! `on_stop` branch must not also keep the deadline armed once the saga has
//! durably ended: nothing is left to consume the firing but a dead letter,
//! and reaching that dead letter means the manager respawned an instance that
//! has nothing left to do.
//!
//! What got this wrong is observable here: if `on_stop` released the timer
//! only by delivery kind and not by whether the saga had ended, the deadline
//! stays registered with the resident scheduler past `End`, elapses anyway,
//! and the manager respawns the already-ended instance to deliver it — a
//! second lifecycle, and an `ended_saga_received_message` dead letter on the
//! saga's own stream. Correct behaviour cancels the timer when `End` runs, so
//! neither ever happens.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_manager_fronted_end_cancels_live_timer`.

#[path = "common/helpers.rs"]
mod common;
use common::{dead_letter_of, failure_kind, outbox_kind_of, schedule_kind_of};
use common::{JsonCodec, OutboxKind, ScheduleKind};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

/// The one instance every `Ping` correlates to.
const INSTANCE_ID: &str = "end-cancels-timer-instance";
const UPSTREAM_KEY: &str = "end-cancels-timer-pings";

/// The scheduled delay.  Short, to keep the test fast, but long enough that
/// `handle` (which schedules it and ends in the same effect) has certainly
/// returned well before it could elapse.
const DEADLINE: Duration = Duration::from_millis(300);

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping;

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("end_cancels_timer"), TypeName::new("Ping"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("end_cancels_timer"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

/// What the deadline carries.  It carries nothing: the timer exists only to be
/// live at the moment `on_stop` runs, and must never fire.
#[derive(Clone, Debug, PartialEq)]
struct Due;

struct DueCodec;

impl Codec<Due> for DueCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Due) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Due, Self::Error> {
        Ok(Due)
    }
}

impl DeadlineMessage for Due {
    type Codec = DueCodec;
}

/// The whole alphabet: one observed source, and the saga's own time.
#[derive(SagaSubscribe)]
enum Observed {
    Ping(Ping),
    #[subscribe(deadline)]
    Due(Due),
}

// Saga under test: schedules a deadline and ends in the same decision, so the
// timer the manager's resident scheduler holds for this instance is live at
// the moment `on_stop` runs.

struct EndingSaga {
    deadline: Deadline<Due>,
}

#[async_trait]
impl Saga for EndingSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Ping(_) => Some(SagaId::new(INSTANCE_ID)),
            // A firing is addressed to the instance that set it and never
            // travels the subscription path.
            Observed::Due(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Noted>, Self::Error> {
        match input {
            Observed::Ping(_) => Ok(self
                .deadline
                .set(TimerName::new("deadline"), DEADLINE, Due)
                .then_end()),
            // The deadline arm is never exercised here — the live timer must be
            // cancelled before it ever fires — so reacting with nothing is
            // enough, and a firing observed at all is what the assertions
            // below reject.
            Observed::Due(_) => Ok(SagaEffect::None),
        }
    }
}

// Helpers

async fn append_ping(store: &Arc<dyn EventStore>, sequence: u64) {
    let payload = serde_json::to_vec(&Ping)
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Poll the saga stream until its durable `Ended` outbox marker is present, or
/// the timeout elapses.  Returns the full stream snapshot so the caller can
/// make additional assertions on what led up to it.
async fn wait_for_ended_marker(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events
            .iter()
            .filter_map(outbox_kind_of)
            .any(|kind| matches!(kind, OutboxKind::Ended(_)))
        {
            return events;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for the saga's own `Ended` outbox marker; \
                 stream held: {:?}",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Test

/// Given a manager-fronted instance whose single decision both schedules a
/// deadline and ends the saga,
/// when the saga has durably ended,
/// then the deadline must already be released from the resident scheduler, so
/// waiting past it produces neither a second instance (the manager restoring
/// an ended saga to deliver a firing it cannot use) nor the
/// `ended_saga_received_message` dead letter that delivery would leave behind.
#[tokio::test]
async fn terminal_end_cancels_the_live_timer_so_it_never_fires() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(INSTANCE_ID);

    let instances_spawned = Arc::new(AtomicUsize::new(0));
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy =
        SagaManagerProps::<EndingSaga>::new(Arc::clone(&saga_store), move |components| {
            spawned_for_producer.fetch_add(1, Ordering::SeqCst);
            EndingSaga {
                deadline: components.deadline(),
            }
        })
        .subscribed_to::<Ping>(Subscription::stream(&upstream_store, UPSTREAM_KEY))
        .spawn(system.process_system())
        .await;

    append_ping(&upstream_store, 1).await;
    let events = wait_for_ended_marker(&saga_store, &saga_id, Duration::from_secs(5)).await;

    assert!(
        events
            .iter()
            .filter_map(schedule_kind_of)
            .any(|kind| matches!(kind, ScheduleKind::Scheduled(_))),
        "the saga must have registered its deadline before ending, or this \
         test exercises no live timer at all; recorded: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        1,
        "exactly one instance may have been built before the deadline elapses"
    );

    // When: the deadline eventually elapsing while the saga is already ended
    // is the condition under test, so the test waits it out rather than
    // synchronising around it.
    tokio::time::sleep(DEADLINE * 5).await;

    // Then
    let events = load_saga_events(&saga_store, &saga_id).await;
    let recorded = || {
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    };

    assert!(
        !events
            .iter()
            .filter_map(schedule_kind_of)
            .any(|kind| matches!(kind, ScheduleKind::Fired(_))),
        "the deadline must never reach `fired` once the saga has durably \
         ended: `on_stop`'s release must remove it from the resident \
         scheduler before it elapses; recorded: {:?}",
        recorded()
    );
    assert!(
        !events
            .iter()
            .filter_map(dead_letter_of)
            .any(|dl| failure_kind(&dl.failure) == "ended_saga_received_message"),
        "no `ended_saga_received_message` dead letter may appear: that record \
         only exists when a respawned instance received the firing the \
         cancelled timer should have prevented; recorded: {:?}",
        recorded()
    );
    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        1,
        "a timer released on the terminal stop must never respawn the \
         already-ended instance to deliver a firing it can only dead-letter; \
         recorded: {:?}",
        recorded()
    );
}
