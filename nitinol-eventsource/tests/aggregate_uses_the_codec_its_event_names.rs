//! An activation writes and replays through the codec its event type names,
//! and nothing has to hand it one.
//!
//! This is the acceptance scenario "an aggregate runs on its named codec
//! alone": props are built
//! and spawned with no codec step anywhere in the chain, and both directions of
//! the stream still work.
//!
//! A round-trip alone would not distinguish "the named codec ran" from "some
//! codec ran consistently in both directions", so each fixture codec writes an
//! unmistakable tag and refuses payloads that lack it. The tests then read the
//! raw bytes out of the store and judge them:
//!
//! - one aggregate's stream must carry *its* event's tag;
//! - two aggregates spawned from one system must carry *different* tags, which
//!   is what a single system-wide codec could not produce.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test aggregate_uses_the_codec_its_event_names`.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, Query,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixture: an event and the codec it names.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Incremented {
    by: u64,
}

/// Tagged so a stream can be read back and attributed to this codec and no
/// other.
struct IncrementedCodec;

impl IncrementedCodec {
    const TAG: &'static [u8] = b"inc/";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not an `inc/` record")]
struct NotAnIncrement;

impl Codec<Incremented> for IncrementedCodec {
    type Error = NotAnIncrement;

    fn encode(event: &Incremented) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(&event.by.to_be_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<Incremented, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(NotAnIncrement)?;
        let by = body
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| NotAnIncrement)?;
        Ok(Incremented { by })
    }
}

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("named.codec"), TypeName::new("Incremented"));

    type Codec = IncrementedCodec;
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, event: Incremented) {
        self.value += event.by;
    }
}

struct Increment(u64);
struct GetCount;

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented { by: cmd.0 }]).output(())
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixture: a second event type, naming a different codec.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Labelled {
    label: String,
}

struct LabelledCodec;

impl LabelledCodec {
    const TAG: &'static [u8] = b"lbl/";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not an `lbl/` record")]
struct NotALabel;

impl Codec<Labelled> for LabelledCodec {
    type Error = NotALabel;

    fn encode(event: &Labelled) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(event.label.as_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<Labelled, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(NotALabel)?;
        let label = std::str::from_utf8(body).map_err(|_| NotALabel)?.to_owned();
        Ok(Labelled { label })
    }
}

impl Event for Labelled {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("named.codec"), TypeName::new("Labelled"));

    type Codec = LabelledCodec;
}

#[derive(Default)]
struct Nameplate {
    label: String,
}

impl Aggregate for Nameplate {
    type Event = Labelled;

    fn apply(&mut self, event: Labelled) {
        self.label = event.label;
    }
}

struct Relabel(&'static str);
struct GetLabel;

impl Decider<Relabel> for Nameplate {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Relabel) -> Decision<Labelled, (), Self::Rejection> {
        Decision::persist(vec![Labelled {
            label: cmd.0.to_owned(),
        }])
        .output(())
    }
}

impl Query<GetLabel> for Nameplate {
    type Response = String;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetLabel) -> Result<String, Self::Error> {
        Ok(self.label.clone())
    }
}

// Helpers

/// The payloads stored under `id`, in stream order, exactly as the activation
/// wrote them.
async fn stored_payloads(store: &Arc<dyn EventStore>, id: &AggregateId) -> Vec<Bytes> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.into_iter().map(|event| event.payload).collect()
}

/// Props with no codec step: `new` straight to `spawn`.
async fn spawn_counter(
    system: &ProcessSystem,
    id: AggregateId,
    store: Arc<dyn EventStore>,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id, store)
        .spawn(system)
        .await
}

// Appending

/// Given props built without any codec step, When two commands are asked,
/// Then the stream holds exactly the bytes `<Incremented as Event>::Codec`
/// produces — so the codec the event names is the one that wrote them.
#[tokio::test]
async fn an_append_writes_the_bytes_the_events_codec_produces() {
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("named-codec-append");

    let proxy = spawn_counter(&system, id.clone(), Arc::clone(&store)).await;
    proxy.ask(Increment(7)).await.expect("first ask");
    proxy.ask(Increment(5)).await.expect("second ask");

    let payloads = stored_payloads(&store, &id).await;

    let expected: Vec<Bytes> = [7u64, 5]
        .into_iter()
        .map(|by| {
            <IncrementedCodec as Codec<Incremented>>::encode(&Incremented { by })
                .expect("the fixture event must encode")
        })
        .collect();

    assert_eq!(
        payloads, expected,
        "the stream must hold what the codec named by `Incremented` writes"
    );
    assert!(
        payloads
            .iter()
            .all(|payload| payload.starts_with(IncrementedCodec::TAG)),
        "every payload must carry the named codec's tag"
    );
}

// Replaying

/// Given a stream written by an activation that was never handed a codec,
/// When a second activation of the same aggregate starts, Then it replays the
/// stream through the codec the event names and reaches the same state.
#[tokio::test]
async fn a_replay_reads_the_stream_through_the_events_codec() {
    let system = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("named-codec-replay");

    {
        let writer = spawn_counter(&system, id.clone(), Arc::clone(&store)).await;
        writer.ask(Increment(7)).await.expect("first ask");
        writer.ask(Increment(5)).await.expect("second ask");
    }

    let replayed = spawn_counter(&system, id, store).await;
    let value = replayed.exec(GetCount).await.expect("exec must succeed");

    assert_eq!(
        value, 12,
        "the replayed activation must have decoded both events"
    );
}

// Per-event resolution

/// Given one system built with no codec of its own, When two aggregates whose
/// events name different codecs both write through it, Then each stream carries
/// its own event's format — the resolution is per event type, not per system.
#[tokio::test]
async fn two_event_types_on_one_system_each_use_their_own_codec() {
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // No `with_codec` step: the builder binds a store and nothing else.
    let system = EventSourceSystem::builder(ps)
        .with_event_store(Arc::clone(&store))
        .build();

    let counter_id = AggregateId::new("named-codec-counter");
    let nameplate_id = AggregateId::new("named-codec-nameplate");

    let counter = system.spawn_aggregate::<Counter>(counter_id.clone()).await;
    let nameplate = system
        .spawn_aggregate::<Nameplate>(nameplate_id.clone())
        .await;

    counter.ask(Increment(3)).await.expect("increment");
    nameplate.ask(Relabel("payroll")).await.expect("relabel");

    let counter_payloads = stored_payloads(&store, &counter_id).await;
    let nameplate_payloads = stored_payloads(&store, &nameplate_id).await;

    assert_eq!(
        counter_payloads,
        vec![
            <IncrementedCodec as Codec<Incremented>>::encode(&Incremented { by: 3 })
                .expect("the fixture event must encode")
        ],
        "the counter's stream must be written by the codec `Incremented` names"
    );
    assert_eq!(
        nameplate_payloads,
        vec![<LabelledCodec as Codec<Labelled>>::encode(&Labelled {
            label: String::from("payroll")
        })
        .expect("the fixture event must encode")],
        "the nameplate's stream must be written by the codec `Labelled` names"
    );

    assert_eq!(
        counter.exec(GetCount).await.expect("exec must succeed"),
        3,
        "the counter must still answer from its own decoded state"
    );
    assert_eq!(
        nameplate.exec(GetLabel).await.expect("exec must succeed"),
        "payroll",
        "the nameplate must still answer from its own decoded state"
    );
}
