//! A projector subscribes to an event type by naming it, and the bytes it is
//! handed are decoded by the codec that event type names.
//!
//! `with_event::<E>()` takes no codec. The type list stays explicit — which
//! events a projection follows is a design statement, not something to be
//! inferred — but what reads them is settled by `E` itself.
//!
//! Two subscribed event types carry mutually unreadable formats here, and both
//! payloads carry data rather than being empty. A projector wired to one shared
//! codec would fail to decode one of them, and a projector that decoded nothing
//! would deliver empty values; either shows up as a difference in what
//! `project` recorded.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test projector_uses_the_codec_its_event_names`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{Event, ProjectionContext, Projector, ProjectorProps};
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, ProjectionId, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixture: a counted event, encoded as a big-endian number behind a tag.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Counted {
    by: u64,
}

struct CountedCodec;

impl CountedCodec {
    const TAG: &'static [u8] = b"c/";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not a `c/` record")]
struct NotCounted;

impl Codec<Counted> for CountedCodec {
    type Error = NotCounted;

    fn encode(event: &Counted) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(&event.by.to_be_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<Counted, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(NotCounted)?;
        let by = body
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| NotCounted)?;
        Ok(Counted { by })
    }
}

impl Event for Counted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("named.projection"), TypeName::new("Counted"));

    type Codec = CountedCodec;
}

// Fixture: a labelled event, encoded as text behind a different tag.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Labelled {
    label: String,
}

struct LabelledCodec;

impl LabelledCodec {
    const TAG: &'static [u8] = b"l/";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not an `l/` record")]
struct NotLabelled;

impl Codec<Labelled> for LabelledCodec {
    type Error = NotLabelled;

    fn encode(event: &Labelled) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(event.label.as_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<Labelled, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(NotLabelled)?;
        let label = std::str::from_utf8(body)
            .map_err(|_| NotLabelled)?
            .to_owned();
        Ok(Labelled { label })
    }
}

impl Event for Labelled {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("named.projection"), TypeName::new("Labelled"));

    type Codec = LabelledCodec;
}

// Fixture: a projector that records the values it was handed.

/// What `project` received, in arrival order, so the test judges decoded values
/// rather than a call count.
#[derive(Clone, Default)]
struct Recorded {
    counted: Arc<Mutex<Vec<Counted>>>,
    labelled: Arc<Mutex<Vec<Labelled>>>,
    notify: Arc<Notify>,
}

impl Recorded {
    fn counted(&self) -> Vec<Counted> {
        self.counted
            .lock()
            .expect("the recorder mutex is never poisoned")
            .clone()
    }

    fn labelled(&self) -> Vec<Labelled> {
        self.labelled
            .lock()
            .expect("the recorder mutex is never poisoned")
            .clone()
    }

    fn total(&self) -> usize {
        self.counted().len() + self.labelled().len()
    }

    /// Block until `expected` events have been projected, driven by the
    /// projector's own notification rather than by a fixed wait.
    async fn wait_for(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.notify.notified();
                if self.total() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {expected} projections; saw {}",
                self.total()
            )
        });
    }
}

#[async_trait]
impl Projector<Counted> for Recorded {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        event: Counted,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.counted
            .lock()
            .expect("the recorder mutex is never poisoned")
            .push(event);
        self.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl Projector<Labelled> for Recorded {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        event: Labelled,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.labelled
            .lock()
            .expect("the recorder mutex is never poisoned")
            .push(event);
        self.notify.notify_waiters();
        Ok(())
    }
}

// Helpers

/// Seed the store with bytes produced by the codec the event type names, which
/// is what an activation of that event's aggregate would have written.
async fn seed(
    store: &InMemoryEventStore,
    agg_id: &AggregateId,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

// Subscribing

/// Given two subscribed event types whose codecs cannot read each other's
/// bytes, When a projector is wired with `with_event::<E>()` and no codec
/// argument, Then each event arrives decoded by the codec its own type names.
#[tokio::test]
async fn each_subscribed_event_is_decoded_by_the_codec_it_names() {
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("named-projection-source");

    seed(
        &event_store,
        &agg_id,
        1,
        Counted::EVENT_TYPE,
        <CountedCodec as Codec<Counted>>::encode(&Counted { by: 9 })
            .expect("the fixture event must encode"),
    )
    .await;
    seed(
        &event_store,
        &agg_id,
        2,
        Labelled::EVENT_TYPE,
        <LabelledCodec as Codec<Labelled>>::encode(&Labelled {
            label: String::from("payroll"),
        })
        .expect("the fixture event must encode"),
    )
    .await;

    let recorded = Recorded::default();
    let produced = recorded.clone();

    let persistence = Persistence::new(Arc::clone(&event_store) as Arc<dyn EventStore>)
        .with_projection_checkpoints(Arc::clone(&checkpoint_store));
    let _proxy = ProjectorProps::new(
        ProjectionId::new("named-projection"),
        &persistence,
        move || produced.clone(),
    )
    .with_event::<Counted>()
    .with_event::<Labelled>()
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    recorded.wait_for(2).await;

    assert_eq!(
        recorded.counted(),
        vec![Counted { by: 9 }],
        "the counted event must arrive carrying the number its codec wrote"
    );
    assert_eq!(
        recorded.labelled(),
        vec![Labelled {
            label: String::from("payroll")
        }],
        "the labelled event must arrive carrying the text its codec wrote"
    );
}
