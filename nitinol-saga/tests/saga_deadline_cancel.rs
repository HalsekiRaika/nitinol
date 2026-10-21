//! A cancelled deadline stays cancelled across a restart.
//!
//! ```gherkin
//!   Scenario: a cancelled deadline is not re-armed by replay
//!     Given a saga stream holding a schedule fact and its cancellation
//!     When a fresh incarnation replays that stream
//!     Then the cancelled deadline is not re-armed and never fires
//! ```
//!
//! ## Why this is a separate claim from cancelling a live timer
//!
//! `saga_deadline_scheduler.rs` cancels a deadline the same incarnation set, so
//! the in-memory timer is what gets torn down.  The durable half is what this
//! file is about: the `cancelled` marker has to be folded out of the stream on
//! replay, or a restart re-arms every deadline that was ever set.  The two fail
//! independently — a cancel that only reaches the timer passes there and fails
//! here.
//!
//! ## Proving a negative without a sleep
//!
//! A second, uncancelled deadline under a different name is seeded with the
//! same delay and acts as the barrier: once it has fired, replay's re-arming is
//! over, so the cancelled one's absence is already decided.
//!
//! ## API contract assumed by this file
//!
//! - `DeadlineMessage`, `SagaComponents::deadline`, `Deadline::set` /
//!   `Deadline::cancel`, and `#[subscribe(deadline)]` — see
//!   `saga_deadline_scheduler.rs` for the full assumed surface.
//! - Schedule markers are appended under `nitinol.saga.schedule`; the
//!   transition a marker records is carried by its payload's `oneof`, whose
//!   `cancelled` arm carries the saga id and the timer name.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_deadline_cancel`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_consumed_fact, encode_schedule_scheduled, CancelledMarkerPayload, ScheduleKind,
    ScheduleMarkerPayload, CONSUMED_MARKER, SCHEDULE_MARKER,
};

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
    Subscription, TimerName,
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
struct Trigger {
    name: String,
    after_millis: u64,
    cancel: bool,
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_cancel"), TypeName::new("Trigger"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_cancel"), TypeName::new("Recorded"));
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
enum CancelInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

const SAGA_ID: &str = "deadline-cancel-saga";
const DOOMED: &str = "doomed";
const BARRIER: &str = "barrier";

struct CancellingSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Saga for CancellingSaga {
    type Subscribe = CancelInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            CancelInput::Trigger(_) => Some(SagaId::new(SAGA_ID)),
            CancelInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            CancelInput::Trigger(trigger) if trigger.cancel => {
                Ok(self.deadline.cancel(TimerName::new(trigger.name)))
            }
            CancelInput::Trigger(trigger) => Ok(self.deadline.set(
                TimerName::new(trigger.name.clone()),
                Duration::from_millis(trigger.after_millis),
                Reminder {
                    label: trigger.name,
                },
            )),
            CancelInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect("fired mutex")
                    .push(reminder.label.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.label),
                }))
            }
        }
    }
}

// Helpers

/// Mirror of the `cancelled` arm, built from the shared prost mirror so the
/// seeded row is field-for-field what `ScheduleEvent::Cancelled` writes.
fn encode_schedule_cancelled(saga_id: &SagaId, name: &TimerName, at_unix_millis: i64) -> Bytes {
    let msg = ScheduleMarkerPayload {
        kind: Some(ScheduleKind::Cancelled(CancelledMarkerPayload {
            saga_id: saga_id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            cancelled_at_unix_millis: at_unix_millis,
        })),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

async fn seed_row(store: &Journal, saga_id: &SagaId, sequence: u64, payload: Bytes) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: SCHEDULE_MARKER,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a schedule row must succeed");
}

fn scheduled_row(saga_id: &SagaId, name: &str, after: Duration) -> Bytes {
    let message = Reminder {
        label: name.to_owned(),
    };
    let payload = <ReminderCodec as Codec<Reminder>>::encode(&message)
        .expect("encoding a Reminder must succeed");
    encode_schedule_scheduled(
        saga_id,
        &TimerName::new(name),
        after,
        Reminder::TYPE_NAME,
        Reminder::CODEC_NAME,
        &payload,
        jiff::Timestamp::now().as_millisecond(),
    )
}

/// Pair an upstream record with a position already naming it, so its arrival
/// wakes an instance without deciding anything.
///
/// A host spawns an instance when a record that instance's correlation claims
/// arrives, so a test whose subject is replay alone still needs one record to
/// arrive.  Its position is already on the stream, so it is dropped before
/// `Saga::handle` and decides nothing.
///
/// What it *would* decide is the discriminator: it cancels the very deadline
/// the barrier below waits for, so a record that did reach `handle` could not
/// be mistaken for one that did not.
async fn seed_an_inert_boot_record(
    journal: &Journal,
    saga_id: &SagaId,
    saga_sequence: u64,
    upstream_key: &str,
) {
    journal
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: saga_sequence,
                event_type: CONSUMED_MARKER,
                payload: encode_consumed_fact(upstream_key, 1, &Trigger::EVENT_TYPE.to_string()),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a consumed position must succeed");

    let payload = serde_json::to_vec(&Trigger {
        name: BARRIER.to_owned(),
        after_millis: 0,
        cancel: true,
    })
    .map(Bytes::from)
    .expect("encode Trigger must succeed");
    journal
        .append(
            upstream_key,
            vec![AppendingEvent {
                sequence: 1,
                event_type: Trigger::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending the boot record must succeed");
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_fired(fired: &Arc<Mutex<Vec<String>>>, label: &str) -> Vec<String> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = fired.lock().expect("fired mutex").clone();
        if current.iter().any(|seen| seen == label) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the `{label}` deadline to fire; fired={current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Tests

/// Given a saga stream holding a schedule fact and its cancellation,
/// When a fresh incarnation replays that stream,
/// Then the cancelled deadline is not re-armed, while an uncancelled one is.
#[tokio::test]
async fn replay_does_not_rearm_a_cancelled_deadline() {
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    // One journal: the saga's own stream and the upstream boot record are
    // tenants of the store the system distributes, kept apart by stream key.
    let journal = system.persistence().event_store();
    let saga_id = SagaId::new(SAGA_ID);

    seed_row(
        &journal,
        &saga_id,
        1,
        scheduled_row(&saga_id, DOOMED, Duration::from_millis(400)),
    )
    .await;
    seed_row(
        &journal,
        &saga_id,
        2,
        scheduled_row(&saga_id, BARRIER, Duration::from_millis(400)),
    )
    .await;
    seed_row(
        &journal,
        &saga_id,
        3,
        encode_schedule_cancelled(
            &saga_id,
            &TimerName::new(DOOMED),
            jiff::Timestamp::now().as_millisecond(),
        ),
    )
    .await;

    // One already-consumed upstream record, so there is an instance at all;
    // replay of the seeded facts is still the only way a deadline can be armed
    // in this incarnation.
    seed_an_inert_boot_record(&journal, &saga_id, 4, "deadline-cancel-upstream-boot").await;
    let fired = Arc::new(Mutex::new(Vec::<String>::new()));

    let fired_for_producer = Arc::clone(&fired);
    let _host = SagaManagerProps::<CancellingSaga>::new(move |components| CancellingSaga {
        deadline: components.deadline(),
        fired: Arc::clone(&fired_for_producer),
    })
    .observing::<Trigger>(Subscription::stream("deadline-cancel-upstream-boot"))
    .spawn(&system)
    .await;

    let seen = wait_for_fired(&fired, BARRIER).await;

    assert_eq!(
        seen,
        vec![BARRIER.to_owned()],
        "replay must re-arm the deadline that was never cancelled and leave the \
         cancelled one alone; a cancellation folded out of the stream is the \
         only durable record there is that it was cancelled; fired={seen:?}"
    );
}
