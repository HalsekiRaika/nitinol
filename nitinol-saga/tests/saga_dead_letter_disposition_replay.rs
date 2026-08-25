//! Disposition markers written by the DLQ pull API must stay invisible to the
//! saga's own replay: classified as a framework marker, folded into nothing.
//!
//! The pull API appends its `processed` / `evicted` markers onto the saga's
//! **own** stream, the same stream replay reads on every start.  Two things
//! must therefore hold, and neither is visible from the store alone:
//!
//! 1. The marker is never handed to the saga's domain codec.  This file uses a
//!    codec that accepts *any* bytes, so a marker that reached the domain
//!    branch would decode successfully and land in `Saga::apply` — which is
//!    exactly what a codec that rejects the bytes (JSON, say) would hide
//!    behind a skipped-and-logged record.
//! 2. The marker folds into no journal fact.  In particular it must not look
//!    like termination: a saga that replays a marker and comes back `ended`
//!    would silently stop reacting to upstream events.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    appending_system_event, system::EventSourceSystem, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, DeadLetterQueue, Saga, SagaContext, SagaEffect, SagaFailure, SagaId,
    SagaProps, SourceContext,
};

// Codecs

/// Upstream codec — ordinary JSON.
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

/// The saga's own codec, deliberately **total**: every byte string decodes.
///
/// A codec that can fail masks misclassification — a framework marker routed
/// into the domain branch merely gets logged and skipped, and the saga looks
/// healthy.  With this codec the misclassification becomes an extra
/// `Saga::apply` call, which the tests below can see.
struct AnyBytesCodec;

impl Codec<Note> for AnyBytesCodec {
    type Error = Infallible;

    fn encode(event: &Note) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(event.text.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Note, Self::Error> {
        Ok(Note {
            text: String::from_utf8_lossy(payload).into_owned(),
        })
    }
}

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping;

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_disp"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, PartialEq)]
struct Note {
    text: String,
}

impl Event for Note {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_disp"), TypeName::new("Note"));
}

/// Correlation rule of [`NoteSaga`]: one instance owns every `Ping`.
const NOTE_SAGA_ID: &str = "disposition-replay-saga";

struct NoteSaga {
    applied: Arc<Mutex<Vec<Note>>>,
}

#[async_trait]
impl Saga for NoteSaga {
    type SubscribedEvent = Ping;
    type Event = Note;
    type Error = Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Ping) -> Option<SagaId> {
        Some(SagaId::new(NOTE_SAGA_ID))
    }

    fn apply(&mut self, event: Note) {
        self.applied
            .lock()
            .expect("applied mutex is never poisoned: no holder panics while the guard is alive")
            .push(event);
    }

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Note>, Self::Error> {
        Ok(SagaEffect::persist(Note {
            text: "handled".to_owned(),
        }))
    }
}

// Helpers

async fn seed_dead_letter(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
    let event = DeadLetterEvent {
        seq: sequence,
        saga_id: saga_id.clone(),
        failure: SagaFailure::HandleFailed {
            error: "seeded failure".to_owned(),
        },
        occurred_at_unix_millis: 1_700_000_000_000,
        source: SourceContext::without_upstream(),
    };
    store
        .append(
            saga_id.as_str(),
            vec![appending_system_event(
                sequence,
                &event,
                jiff::Timestamp::now(),
            )],
        )
        .await
        .expect("seeding a dead letter must succeed");
}

async fn seed_note(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64, text: &str) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: Note::EVENT_TYPE,
                payload: Bytes::from(text.to_owned().into_bytes()),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a domain note must succeed");
}

async fn append_ping(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload: serde_json::to_vec(&Ping)
                    .map(Bytes::from)
                    .expect("encode Ping must succeed"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Ping must succeed");
}

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn snapshot(applied: &Arc<Mutex<Vec<Note>>>) -> Vec<Note> {
    applied
        .lock()
        .expect("applied mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

// Tests

/// Given a saga stream holding a domain event, a dead letter, and the
/// disposition marker the pull API wrote for that dead letter,
/// When the saga replays that stream under a codec that accepts any bytes,
/// Then `Saga::apply` is called exactly once, with the domain event —
/// the framework records are classified by type and never reach the codec.
#[tokio::test]
async fn replay_never_hands_a_disposition_marker_to_the_domain_codec() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(NOTE_SAGA_ID);

    seed_note(&saga_store, &saga_id, 1, "seeded").await;
    seed_dead_letter(&saga_store, &saga_id, 2).await;
    DeadLetterQueue::new(Arc::clone(&saga_store), saga_id.clone())
        .mark_processed(2)
        .await
        .expect("mark_processed must succeed");

    let applied = Arc::new(Mutex::new(Vec::<Note>::new()));
    let applied_for_saga = Arc::clone(&applied);

    // No upstream events: the only thing this saga does is replay.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let _proxy =
        SagaProps::<NoteSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || NoteSaga {
            applied: Arc::clone(&applied_for_saga),
        })
        .with_codec(Arc::new(AnyBytesCodec))
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "disposition-replay-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while snapshot(&applied).is_empty() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for replay to apply the seeded domain event"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Replay has reached the first record; give the remaining two records of the
    // (three-record) stream time to be folded before fixing the count.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        snapshot(&applied),
        vec![Note {
            text: "seeded".to_owned()
        }],
        "replay must apply exactly the one domain event; the dead letter and the \
         disposition marker must be classified by their event type and never \
         handed to the saga's codec"
    );
}

/// Given a saga stream whose most recent record is a disposition marker,
/// When the saga starts and then receives an upstream event,
/// Then it handles the event and persists at the sequence just past the
/// marker.
///
/// Two independent regressions would break this: folding the marker into the
/// journal's termination fact (the saga would come back `ended` and ignore the
/// upstream event), and failing to count the marker's stream position (the
/// persist would collide with the marker's sequence).
#[tokio::test]
async fn a_disposition_marker_neither_ends_the_saga_nor_desynchronises_its_sequence() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(NOTE_SAGA_ID);

    seed_dead_letter(&saga_store, &saga_id, 1).await;
    DeadLetterQueue::new(Arc::clone(&saga_store), saga_id.clone())
        .evict(1)
        .await
        .expect("evict must succeed");
    // Stream now: dead letter @1, disposition marker @2.

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "disposition-sequence-upstream", 1).await;

    let applied = Arc::new(Mutex::new(Vec::<Note>::new()));
    let applied_for_saga = Arc::clone(&applied);

    let _proxy =
        SagaProps::<NoteSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || NoteSaga {
            applied: Arc::clone(&applied_for_saga),
        })
        .with_codec(Arc::new(AnyBytesCodec))
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "disposition-sequence-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    let deadline = Instant::now() + Duration::from_secs(6);
    let persisted = loop {
        let events = load_stream(&saga_store, &saga_id).await;
        if let Some(event) = events
            .into_iter()
            .find(|e| e.event_type.type_key() == Note::EVENT_TYPE.type_key())
        {
            break event;
        }
        assert!(
            Instant::now() < deadline,
            "the saga must handle the upstream event and persist a domain event — \
             a disposition marker on its stream must not leave it looking ended"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        persisted.sequence, 3,
        "the persisted domain event must land immediately after the disposition \
         marker at sequence 2; replay has to count the marker's stream position \
         even though it folds into no journal fact"
    );
    assert_eq!(
        persisted.payload,
        Bytes::from_static(b"handled"),
        "the persisted record must be the saga's own domain event"
    );
}
