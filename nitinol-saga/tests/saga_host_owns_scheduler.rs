//! Declaring a deadline in the alphabet is the whole declaration: the host
//! resolves the resident timer core from it.
//!
//! ```gherkin
//!   Scenario: a deadline fires with no scheduler wiring
//!     Given a saga whose Subscribe enum declares a #[subscribe(deadline)] variant
//!     And no scheduler is passed to the spawn builder
//!     When handle sets a deadline and it elapses
//!     Then the firing reaches the deadline arm
//!
//!   Scenario: several sagas converge on one timer core
//!     Given two such sagas spawned into one system
//!     Then both deadlines fire
//!     And the system still holds exactly one resident scheduler
//! ```
//!
//! ## What this replaces
//!
//! Scheduling used to be wired per saga.  Leaving the wiring out was not a
//! compile error and not a runtime error either: the schedule fact was still
//! persisted and no timer ever fired, so the saga quietly waited forever for a
//! deadline nobody was keeping.  Resolving the resident core from the alphabet
//! removes the half-wired state rather than reporting it — there is no longer a
//! configuration in which the fact is written and the timer is not armed.
//!
//! The cost of resolving it from a static declaration is an idle timer core for
//! a saga that declares the variant and never sets one, which is free: no
//! deadline means no sleep and no wakeup.
//!
//! ## API contract assumed by this file
//!
//! - No `with_scheduler` anywhere — its absence is pinned in
//!   `saga_with_scheduler_api_removed.rs`.
//! - `SagaManagerProps::new(Fn(SagaComponents<S>) -> S)`,
//!   `observing::<E>(Subscription)`, `spawn(&EventSourceSystem)` — the whole of
//!   the wiring these sagas get, and not one argument of it is a scheduler.
//! - `SchedulerProcess` stays the type key
//!   `ProcessSystem::spawn_resident::<SchedulerProcess>()` resolves.
//! - `DeadlineMessage`, `SagaComponents::deadline`, `Deadline::set`, and
//!   `#[subscribe(deadline)]` — see `saga_deadline_scheduler.rs`.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_host_owns_scheduler`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SchedulerProcess, Subscription, TimerName,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Start {
    label: String,
}

impl Event for Start {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("host_scheduler"), TypeName::new("Start"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("host_scheduler"), TypeName::new("Recorded"));
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
enum HostInput {
    Start(Start),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

struct HostedSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<Mutex<Vec<String>>>,
    saga_id: SagaId,
}

#[async_trait]
impl Saga for HostedSaga {
    type Subscribe = HostInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            // The label *is* the instance id, so two sagas in one system stay
            // separate without a second correlation rule.
            HostInput::Start(start) => Some(SagaId::new(start.label.clone())),
            HostInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            HostInput::Start(start) => Ok(self.deadline.set(
                TimerName::new("due"),
                Duration::from_millis(150),
                Reminder { label: start.label },
            )),
            HostInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect("fired mutex")
                    .push(reminder.label.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("{}:{}", self.saga_id.as_str(), reminder.label),
                }))
            }
        }
    }
}

// Helpers

async fn append_start(store: &Journal, stream_key: &str, label: &str) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence: 1,
                event_type: Start::EVENT_TYPE,
                payload: serde_json::to_vec(&Start {
                    label: label.to_owned(),
                })
                .map(Bytes::from)
                .expect("encode Start must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Start must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_fired(fired: &Arc<Mutex<Vec<String>>>, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = fired.lock().expect("fired mutex").clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} firings; fired={current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Host a deadline-declaring saga with nothing but its producer and its one
/// observed source.  There is no scheduler argument to pass, and no store
/// either: the journal its instances persist to and the one its source is read
/// from are both the system's.
///
/// The manager half of what the spawn started is what these clauses keep: the
/// delivery host beside it carries nothing, because this saga declares no
/// mapping, and dropping its handle stops nothing.
async fn spawn_hosted(
    system: &EventSourceSystem<InMemoryStore>,
    label: &str,
    upstream_key: &str,
    fired: &Arc<Mutex<Vec<String>>>,
) -> nitinol_saga::SagaManagerProxy<HostedSaga> {
    let fired = Arc::clone(fired);
    // The label is the correlation rule, so it is also the id of the one
    // instance this host ever spawns.
    let id_for_producer = SagaId::new(label);
    SagaManagerProps::<HostedSaga>::new(move |components| HostedSaga {
        deadline: components.deadline(),
        fired: Arc::clone(&fired),
        saga_id: id_for_producer.clone(),
    })
    .observing::<Start>(Subscription::stream(upstream_key))
    .spawn(system)
    .await
    .manager()
    .clone()
}

/// A system over a store that supplies every slot, the saga-offset one a
/// delivery host records in included.
async fn system() -> EventSourceSystem<InMemoryStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build()
}

// Tests

/// Given a saga whose alphabet declares a deadline and whose spawn passes no
/// scheduler,
/// When it sets a deadline and the delay elapses,
/// Then the firing arrives: there is no wiring left to forget.
#[tokio::test]
async fn a_deadline_fires_without_any_scheduler_wiring() {
    let system = system().await;

    let journal = system.persistence().event_store();
    append_start(&journal, "host-upstream-1", "host-saga-1").await;

    let fired = Arc::new(Mutex::new(Vec::<String>::new()));
    let _proxy = spawn_hosted(&system, "host-saga-1", "host-upstream-1", &fired).await;

    assert_eq!(
        wait_for_fired(&fired, 1).await,
        vec!["host-saga-1".to_owned()],
        "a saga that declares its own time must get a timer core from the host; \
         the old half-wired state persisted the fact and armed nothing"
    );
}

/// Given two deadline-declaring sagas spawned into one system,
/// When both deadlines elapse,
/// Then both fire, and the system still resolves exactly one resident
/// scheduler: the host's resolution converges rather than starting a core per
/// saga.
#[tokio::test]
async fn several_sagas_converge_on_one_resident_timer_core() {
    let system = system().await;
    let ps = system.process_system();

    // Asked before any saga exists, so the comparison below is against the core
    // the system already held.
    let before = ps.spawn_resident::<SchedulerProcess>().await;

    let journal = system.persistence().event_store();
    append_start(&journal, "host-upstream-a", "host-saga-a").await;
    append_start(&journal, "host-upstream-b", "host-saga-b").await;

    let fired = Arc::new(Mutex::new(Vec::<String>::new()));
    let _first = spawn_hosted(&system, "host-saga-a", "host-upstream-a", &fired).await;
    let _second = spawn_hosted(&system, "host-saga-b", "host-upstream-b", &fired).await;

    let mut seen = wait_for_fired(&fired, 2).await;
    seen.sort();
    assert_eq!(
        seen,
        vec!["host-saga-a".to_owned(), "host-saga-b".to_owned()],
        "both sagas' deadlines must fire from the one core the host resolved"
    );

    let after = ps.spawn_resident::<SchedulerProcess>().await;
    assert_eq!(
        before.pid(),
        after.pid(),
        "spawning deadline-declaring sagas must resolve the resident scheduler, \
         not start a timer core of its own per saga"
    );
}
