//! A deadline message is persisted in the format it names, not in serde's.
//!
//! ```gherkin
//!   Scenario: the schedule fact holds the codec's bytes
//!     Given a deadline message type whose Codec is not a JSON format
//!     When the saga sets a deadline carrying one
//!     Then the scheduled marker's payload is exactly that codec's output
//!
//!   Scenario: the firing is read back through the same codec
//!     Given a schedule fact holding that codec's bytes
//!     When the deadline fires
//!     Then the deadline arm receives the message those bytes encode
//! ```
//!
//! ## Why a codec and not serde
//!
//! A deadline message is a promise to the saga's future self, and the promise
//! is kept by *persisting* it inside the schedule fact.  Every other persisted
//! value in the framework names the format that writes it down — `Event::Codec`,
//! `Snapshotable::Codec` — so this one does too, through
//! `DeadlineMessage::Codec`.  A `serde_json` step hidden inside the framework
//! would make the schedule fact the one persisted record whose format its own
//! type does not describe.
//!
//! The codec below is deliberately not a JSON shape, so the two candidate
//! implementations are distinguishable on the wire rather than only in the
//! source: a framework that reached for serde would write `{"attempt":…}` where
//! the assertion expects `2|remind-me`.
//!
//! ## API contract assumed by this file
//!
//! - `DeadlineMessage { type Codec: Codec<Self> }`, `SagaComponents::deadline`,
//!   `Deadline::set`, and `#[subscribe(deadline)]` — see
//!   `saga_deadline_scheduler.rs` for the full assumed surface.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_deadline_codec`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_consumed_fact, encode_schedule_scheduled, schedule_kind_of, ScheduleKind,
    CONSUMED_MARKER, SCHEDULE_MARKER,
};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
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
    note: String,
    attempt: u32,
    after_millis: u64,
}

impl Event for Trigger {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_codec"), TypeName::new("Trigger"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded {
    label: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("deadline_codec"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// Also `Serialize`, so the test can show the persisted bytes are *not* what
/// serde would have written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reminder {
    note: String,
    attempt: u32,
}

/// `attempt|note` — a format no serde derive would produce, and one
/// `serde_json::from_slice` cannot read.
struct PipeCodec;

impl Codec<Reminder> for PipeCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(format!("{}|{}", value.attempt, value.note)))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        let text = String::from_utf8_lossy(payload).into_owned();
        let (attempt, note) = text.split_once('|').unwrap_or(("0", text.as_str()));
        Ok(Reminder {
            attempt: attempt.parse().unwrap_or(0),
            note: note.to_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "pipe-codec";
    type Codec = PipeCodec;
}

#[derive(SagaSubscribe)]
enum CodecInput {
    Trigger(Trigger),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

const SAGA_ID: &str = "deadline-codec-saga";

struct CodecSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<Mutex<Vec<Reminder>>>,
}

#[async_trait]
impl Saga for CodecSaga {
    type Subscribe = CodecInput;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            CodecInput::Trigger(_) => Some(SagaId::new(SAGA_ID)),
            CodecInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            CodecInput::Trigger(trigger) => Ok(self.deadline.set(
                TimerName::new("remind"),
                Duration::from_millis(trigger.after_millis),
                Reminder {
                    note: trigger.note,
                    attempt: trigger.attempt,
                },
            )),
            CodecInput::Reminder(reminder) => {
                self.fired
                    .lock()
                    .expect("fired mutex")
                    .push(reminder.clone());
                Ok(SagaEffect::persist(Recorded {
                    label: format!("fired:{}", reminder.note),
                }))
            }
        }
    }
}

// Helpers

const NOTE: &str = "remind-me";
const ATTEMPT: u32 = 2;

/// The one message every test in this file sets and expects back.
fn reminder() -> Reminder {
    Reminder {
        note: NOTE.to_owned(),
        attempt: ATTEMPT,
    }
}

async fn append_trigger(store: &Journal, stream_key: &str, after_millis: u64) {
    let message = reminder();
    let payload = serde_json::to_vec(&Trigger {
        note: message.note,
        attempt: message.attempt,
        after_millis,
    })
    .map(Bytes::from)
    .expect("encode Trigger must succeed");
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence: 1,
                event_type: Trigger::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Trigger must succeed");
}

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// The payloads of the `scheduled` markers on this stream, in write order.
fn scheduled_payloads(events: &[LoadedEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(schedule_kind_of)
        .filter_map(|kind| match kind {
            ScheduleKind::Scheduled(marker) => Some(marker.payload),
            _ => None,
        })
        .collect()
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_scheduled_payload(store: &Journal, saga_id: &SagaId) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let payloads = scheduled_payloads(&load_saga_events(store, saga_id).await);
        if !payloads.is_empty() {
            return payloads;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a `scheduled` marker on the saga's own stream"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_fired(fired: &Arc<Mutex<Vec<Reminder>>>) -> Vec<Reminder> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = fired.lock().expect("fired mutex").clone();
        if !current.is_empty() {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the deadline to fire"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn spawn_codec_saga(
    ps: &ProcessSystem,
    saga_store: &Journal,
    upstream_store: &Journal,
    upstream_key: &str,
    fired: &Arc<Mutex<Vec<Reminder>>>,
) -> nitinol_saga::SagaManagerProxy<CodecSaga> {
    let fired = Arc::clone(fired);
    SagaManagerProps::<CodecSaga>::new(saga_store.clone(), move |components| CodecSaga {
        deadline: components.deadline(),
        fired: Arc::clone(&fired),
    })
    .observing::<Trigger>(Subscription::stream(upstream_store, upstream_key))
    .spawn(ps)
    .await
}

/// Pair an upstream record with a position already naming it, so its arrival
/// wakes an instance without deciding anything.
///
/// A host spawns an instance when a record that instance's correlation claims
/// arrives, so a test whose subject is replay alone still needs one record to
/// arrive.  Its position is already on the stream, so it is dropped before
/// `Saga::handle` and sets no deadline of its own.
///
/// The delay it would have set is far out of reach of any barrier below, and
/// under the same `TimerName` as the seeded fact — so a record that *did*
/// reach `handle` would supersede the deadline under test rather than quietly
/// reproduce it.
async fn seed_an_inert_boot_record(
    saga_store: &Journal,
    saga_id: &SagaId,
    saga_sequence: u64,
    upstream_store: &Journal,
    upstream_key: &str,
) {
    saga_store
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

    append_trigger(upstream_store, upstream_key, 600_000).await;
}

// Tests

/// Given a deadline message whose `Codec` is a pipe-separated format,
/// When the saga sets a deadline carrying one,
/// Then the `scheduled` marker's payload is exactly that codec's output — not
/// what serde would have written.
#[tokio::test]
async fn the_schedule_fact_carries_the_codec_the_message_type_names() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    append_trigger(&upstream_store, "deadline-codec-upstream-1", 5_000).await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let fired = Arc::new(Mutex::new(Vec::<Reminder>::new()));

    let _proxy = spawn_codec_saga(
        &ps,
        &saga_store,
        &upstream_store,
        "deadline-codec-upstream-1",
        &fired,
    )
    .await;

    let payloads = wait_for_scheduled_payload(&saga_store, &saga_id).await;

    let expected = <PipeCodec as Codec<Reminder>>::encode(&reminder())
        .expect("encoding a Reminder must succeed");
    assert_eq!(
        payloads,
        vec![expected.to_vec()],
        "the schedule fact must hold the bytes the message type's own codec \
         produced"
    );

    let serde_bytes =
        serde_json::to_vec(&reminder()).expect("the message is also Serialize, for contrast");
    assert_ne!(
        payloads[0], serde_bytes,
        "the discriminator: a framework that serialized the message with serde \
         would have written {serde_bytes:?}"
    );
}

/// Given a saga stream already holding a schedule fact whose payload is that
/// codec's bytes,
/// When the deadline fires,
/// Then the deadline arm receives the message those bytes encode: the schedule
/// fact is read back through the same codec it was written with.
#[tokio::test]
async fn a_firing_is_read_back_through_the_codec_the_message_type_names() {
    let ps = ProcessSystem::new().await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    let expected = reminder();
    let payload = <PipeCodec as Codec<Reminder>>::encode(&expected)
        .expect("encoding a Reminder must succeed");

    saga_store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: SCHEDULE_MARKER,
                payload: encode_schedule_scheduled(
                    &saga_id,
                    &TimerName::new("remind"),
                    Duration::from_millis(300),
                    Reminder::TYPE_NAME,
                    Reminder::CODEC_NAME,
                    &payload,
                    jiff::Timestamp::now().as_millisecond(),
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding an unfired schedule fact must succeed");

    // One already-consumed upstream record, so the instance exists at all;
    // replay of the schedule fact is still the only way a deadline gets in.
    let upstream_store = Journal::from(InMemoryEventStore::default());
    seed_an_inert_boot_record(
        &saga_store,
        &saga_id,
        2,
        &upstream_store,
        "deadline-codec-upstream-2-boot",
    )
    .await;
    let fired = Arc::new(Mutex::new(Vec::<Reminder>::new()));

    let _proxy = spawn_codec_saga(
        &ps,
        &saga_store,
        &upstream_store,
        "deadline-codec-upstream-2-boot",
        &fired,
    )
    .await;

    let seen = wait_for_fired(&fired).await;

    assert_eq!(
        seen,
        vec![expected],
        "the firing must deliver the message the schedule fact's bytes encode, \
         decoded through `DeadlineMessage::Codec`; `serde_json::from_slice` \
         cannot read this payload at all"
    );
}
