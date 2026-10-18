//! A snapshot is written and read through the codec its aggregate names, and
//! the snapshot persistor is wired without being handed one.
//!
//! This is the acceptance scenario "snapshots too run on the type-level
//! binding". The
//! framework itself only ever *recovers* from a snapshot — capturing and
//! saving one is the caller's decision and the caller's call — so the two
//! directions are observed at the two places they actually happen:
//!
//! - the save side, where the caller reaches the format through
//!   `<Counter as Snapshotable>::Codec` rather than through an injected handle;
//! - the recover side, where the wiring declares nothing at all — `Counter`'s
//!   own `Recover = Latest` is what asks for the newest snapshot, and the
//!   activation still decodes what was saved.
//!
//! The snapshot codec and the event codec use deliberately incompatible
//! formats, and the snapshot claims a value the event stream alone could never
//! produce. An activation that reached for the wrong codec would fail to decode
//! and fall back to replaying from default state — which is a different, and
//! visibly different, number.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test snapshot_uses_the_codec_its_aggregate_names`.

use bytes::Bytes;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, Latest, Query,
    Snapshotable,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::store::{InMemoryEventStore, InMemorySnapshotStore};
use nitinol_persistence::{AggregateId, EventType, Family, PersistedSnapshot, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixture: the event and its codec.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Incremented;

struct IncrementedCodec;

impl IncrementedCodec {
    const TAG: &'static [u8] = b"e";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not an event record")]
struct NotAnEvent;

impl Codec<Incremented> for IncrementedCodec {
    type Error = NotAnEvent;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from_static(IncrementedCodec::TAG))
    }

    fn decode(payload: &[u8]) -> Result<Incremented, Self::Error> {
        if payload == Self::TAG {
            Ok(Incremented)
        } else {
            Err(NotAnEvent)
        }
    }
}

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("named.snapshot"), TypeName::new("Incremented"));

    type Codec = IncrementedCodec;
}

// Fixture: the snapshot codec, in a format the event codec cannot read.

struct CounterSnapshotCodec;

impl CounterSnapshotCodec {
    const TAG: &'static [u8] = b"snap/";
}

#[derive(Debug, thiserror::Error)]
#[error("payload is not a `snap/` record")]
struct NotASnapshot;

impl Codec<u64> for CounterSnapshotCodec {
    type Error = NotASnapshot;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        let mut out = Vec::from(Self::TAG);
        out.extend_from_slice(&value.to_be_bytes());
        Ok(Bytes::from(out))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        let body = payload.strip_prefix(Self::TAG).ok_or(NotASnapshot)?;
        body.try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| NotASnapshot)
    }
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for Counter {
    type Snapshot = u64;
    type Codec = CounterSnapshotCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

struct Increment;
struct GetCount;

impl Decider<Increment> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Helpers

/// A system over `store`, on a process system of its own.
///
/// A second activation of one aggregate is a second system over the same store:
/// one system resolves an identity to one activation, so asking the same system
/// again would reach the resident state instead of recovering from the
/// snapshot.
async fn system_over<St: Store>(store: St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// Props with neither a codec step nor a snapshot codec argument, and no
/// persistor to hand over: `Counter`'s own `Recover = Latest` is what asks the
/// system for the one it distributes.
async fn spawn_counter<St: SnapshotSlot>(
    system: &EventSourceSystem<St>,
    id: AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id).spawn(system).await
}

/// The save side as a caller performs it: capture a domain value, then reach
/// the format through the aggregate's own associated type.
fn encode_capture(counter: &Counter) -> Bytes {
    <<Counter as Snapshotable>::Codec as Codec<u64>>::encode(&counter.capture())
        .expect("the snapshot must encode through the codec the aggregate names")
}

// The save side

/// Given a caller that captures a snapshot, When it encodes through
/// `<Counter as Snapshotable>::Codec`, Then the bytes carry the named snapshot
/// codec's tag rather than the event codec's.
#[test]
fn a_caller_reaches_the_snapshot_format_through_the_aggregates_codec() {
    let counter = Counter { value: 42 };

    let payload = encode_capture(&counter);

    assert!(
        payload.starts_with(CounterSnapshotCodec::TAG),
        "the snapshot must be written by the codec `Counter` names"
    );
    assert!(
        <IncrementedCodec as Codec<Incremented>>::decode(&payload).is_err(),
        "the event codec must not be able to read a snapshot payload, so a \
         restore that used it could not be mistaken for a success"
    );
}

// The recover side

/// Given a snapshot claiming a value the two persisted events could never
/// produce, When a new activation starts with nothing wired by hand, Then it
/// recovers that value — the named snapshot codec decoded it.
#[tokio::test]
async fn an_activation_restores_through_the_codec_the_aggregate_names() {
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build();
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("named-snapshot-restore");

    {
        let writer = spawn_counter(&writing, id.clone()).await;
        writer.ask(Increment).await.expect("first ask");
        writer.ask(Increment).await.expect("second ask");
    }

    // 100 is unreachable by replaying the two events above, so the number the
    // next activation answers with says which path it took.  The persistor is
    // the one the system distributes — the same process the activation below
    // loads through.
    writing
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: 2,
            payload: encode_capture(&Counter { value: 100 }),
            created_at: jiff::Timestamp::now(),
        })
        .await
        .expect("saving the snapshot must succeed");

    let restarted = system_over(store).await;
    let restored = spawn_counter(&restarted, id).await;
    let value = restored.exec(GetCount).await.expect("exec must succeed");

    assert_eq!(
        value, 100,
        "the activation must have decoded the snapshot with `Counter::Codec`; \
         2 would mean it ignored the snapshot and replayed from default state"
    );
}

/// Given a snapshot followed by a further event, When a new activation starts,
/// Then it recovers from the snapshot through the named codec and replays only
/// the delta through the event's own codec — the two formats are read by the
/// two codecs that own them.
#[tokio::test]
async fn a_restore_and_a_delta_replay_use_their_own_codecs() {
    let store = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .build();
    let writing = system_over(store.clone()).await;
    let id = AggregateId::new("named-snapshot-delta");

    {
        let writer = spawn_counter(&writing, id.clone()).await;
        writer.ask(Increment).await.expect("first ask");
        writer.ask(Increment).await.expect("second ask");

        writing
            .snapshot_persistor()
            .await
            .save(PersistedSnapshot {
                aggregate_id: id.clone(),
                sequence: 2,
                payload: encode_capture(&Counter { value: 100 }),
                created_at: jiff::Timestamp::now(),
            })
            .await
            .expect("saving the snapshot must succeed");

        writer.ask(Increment).await.expect("third ask");
    }

    let restarted = system_over(store).await;
    let restored = spawn_counter(&restarted, id).await;
    let value = restored.exec(GetCount).await.expect("exec must succeed");

    assert_eq!(
        value, 101,
        "100 from the snapshot plus the one event written after it"
    );
}
