//! `AggregateProxy::snapshot`: the activation saves its own state, at its own
//! sequence, through the codec its aggregate names and the persistor its system
//! distributes.
//!
//! The caller hands over nothing — no payload, no sequence, no store — so what
//! these tests observe is what the activation chose: the sequence `snapshot()`
//! answers with, the row the system's persistor reads back, and what a later
//! activation recovers from it.
//!
//! The snapshot codec writes a tagged format the event codec cannot read, so a
//! payload that decodes through `<Counter as Snapshotable>::Codec` was written
//! by that codec and not by some other encoding of the same number.
//!
//! That `snapshot()` does not exist on a `Recover = Never` aggregate is a
//! compile-time property, fixed by `tests/ui.rs`.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration aggregate_proxy_snapshot::`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProxy, Decider, Decision, Event, ExecError, Latest, Query,
    SnapshotSaveError, Snapshotable,
};
use nitinol_persistence::error::{AppendError, LoadError, SnapshotError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemorySnapshotStore, InMemoryStore,
    SnapshotStore,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot,
    TypeName,
};
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
        Ok(Bytes::from_static(Self::TAG))
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
        EventType::new(Family::new("proxy.snapshot"), TypeName::new("Incremented"));

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

// Fixture: the aggregate.

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

// Fixture: an aggregate whose snapshot codec cannot write.

struct RefusingSnapshotCodec;

#[derive(Debug, thiserror::Error)]
#[error("this codec writes no snapshot")]
struct WritesNothing;

impl Codec<u64> for RefusingSnapshotCodec {
    type Error = WritesNothing;

    fn encode(_value: &u64) -> Result<Bytes, Self::Error> {
        Err(WritesNothing)
    }

    fn decode(_payload: &[u8]) -> Result<u64, Self::Error> {
        Err(WritesNothing)
    }
}

#[derive(Default)]
struct Unencodable;

impl Aggregate for Unencodable {
    type Event = Incremented;
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {}
}

impl Snapshotable for Unencodable {
    type Snapshot = u64;
    type Codec = RefusingSnapshotCodec;

    fn capture(&self) -> u64 {
        0
    }

    fn recover(_snapshot: u64) -> Self {
        Self
    }
}

// Fixture: stores.

const RECORD_LOCK: &str = "the record lock guards only a push or a copy, so no holder panics";

/// An [`EventStore`] that answers from memory and notes the stream sequence of
/// every event a `load` hands out, per stream.
///
/// It is put in front of a restarted system only, so what it records is what
/// that system's activation replayed — not what the writing system read when
/// it started over an empty stream.
#[derive(Clone)]
struct ReplayRecorder {
    inner: InMemoryEventStore,
    yielded: Arc<Mutex<Vec<(String, u64)>>>,
}

impl ReplayRecorder {
    fn over(inner: InMemoryEventStore) -> Self {
        Self {
            inner,
            yielded: Arc::default(),
        }
    }

    /// The stream sequences handed out for `stream_key`, in the order they
    /// were handed out.
    fn replayed(&self, stream_key: &str) -> Vec<u64> {
        self.yielded
            .lock()
            .expect(RECORD_LOCK)
            .iter()
            .filter(|(key, _)| key == stream_key)
            .map(|(_, sequence)| *sequence)
            .collect()
    }
}

#[async_trait]
impl EventStore for ReplayRecorder {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let yielded = Arc::clone(&self.yielded);
        let stream = self.inner.load(query).await?;
        Ok(Box::pin(stream.inspect(move |item| {
            if let Ok(event) = item {
                yielded
                    .lock()
                    .expect(RECORD_LOCK)
                    .push((event.stream_key.clone(), event.sequence));
            }
        })))
    }
}

/// A snapshot store that refuses every save and holds nothing.
#[derive(Clone, Default)]
struct RefusingSnapshotStore;

#[derive(Debug, thiserror::Error)]
#[error("simulated snapshot backend failure")]
struct SimulatedSnapshotFailure;

#[async_trait]
impl SnapshotStore for RefusingSnapshotStore {
    async fn save(&self, _snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        Err(SnapshotError::Backend(Box::new(SimulatedSnapshotFailure)))
    }

    async fn load_latest(
        &self,
        _aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        Ok(None)
    }
}

/// An [`EventStore`] whose first `load` fails, so the first activation over it
/// stops on a failed replay and every later one replays normally.
#[derive(Clone)]
struct FailFirstLoadStore {
    inner: InMemoryEventStore,
    fail_next_load: Arc<AtomicBool>,
}

impl FailFirstLoadStore {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            fail_next_load: Arc::new(AtomicBool::new(true)),
        }
    }
}

#[async_trait]
impl EventStore for FailFirstLoadStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if self.fail_next_load.swap(false, Ordering::SeqCst) {
            return Err(LoadError::Backend(
                "simulated transient backend failure".into(),
            ));
        }
        self.inner.load(query).await
    }
}

// Helpers

/// A system over `store`, on a process system of its own.
///
/// A restart is a second system over the same store: one system resolves an
/// identity to one activation, so asking the same system again would reach the
/// resident state instead of recovering.
async fn system_over<St: Store>(store: St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// The snapshot the system's persistor reads back for `id`.
async fn latest_snapshot<St: SnapshotSlot>(
    system: &EventSourceSystem<St>,
    id: &AggregateId,
) -> PersistedSnapshot {
    system
        .snapshot_persistor()
        .await
        .load_latest(id.clone())
        .await
        .expect("loading the snapshot must succeed")
        .expect("a snapshot must have been saved for the aggregate")
}

fn decode_snapshot(payload: &[u8]) -> u64 {
    <<Counter as Snapshotable>::Codec as Codec<u64>>::decode(payload)
        .expect("the saved payload must be readable by the codec `Counter` names")
}

/// Wait until `proxy`'s activation can no longer be reached.
async fn await_stopped(proxy: &AggregateProxy<Counter>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match proxy.exec(GetCount).await {
                Err(ExecError::Send(_)) => return,
                Ok(_) => tokio::task::yield_now().await,
                Err(e) => panic!("the query must not fail this way: {e:?}"),
            }
        }
    })
    .await
    .expect("the activation whose replay failed must stop")
}

// The save: sequence and codec are the activation's

/// Given a `Latest` Counter that has accepted 5 increments, When `snapshot()`
/// is called, Then it answers 5, the system's persistor reads back a snapshot
/// at sequence 5, and its payload decodes through `Counter`'s own codec to the
/// state the activation holds.
#[tokio::test]
async fn snapshot_saves_at_the_activations_sequence_through_the_aggregates_codec() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("proxy-snapshot-sequence-and-codec");
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    for _ in 0..5 {
        proxy.ask(Increment).await.expect("ask must succeed");
    }

    // When
    let saved = proxy.snapshot().await.expect("snapshot must succeed");

    // Then
    assert_eq!(
        saved, 5,
        "snapshot() must answer the sequence the activation is at"
    );
    let snapshot = latest_snapshot(&system, &id).await;
    assert_eq!(
        snapshot.sequence, 5,
        "the snapshot must be stored at the sequence snapshot() answered with"
    );
    assert_eq!(
        snapshot.aggregate_id, id,
        "the snapshot must be stored under the aggregate's own identity"
    );
    let live = proxy.exec(GetCount).await.expect("exec must succeed");
    assert_eq!(
        decode_snapshot(&snapshot.payload),
        live,
        "the payload must be the activation's captured state, written by `Counter::Codec`"
    );
    assert_eq!(live, 5, "the captured state is the 5 increments");
}

/// Given a snapshot saved through `snapshot()` at sequence 5 and 2 more
/// increments, When a second system over the same store activates the same
/// aggregate, Then it answers 7 and replays only the events at sequences 6 and
/// 7 — the rest came from the snapshot.
#[tokio::test]
async fn a_restart_recovers_from_the_saved_snapshot_and_replays_only_what_followed() {
    // Given
    let events = InMemoryEventStore::default();
    let snapshots = InMemorySnapshotStore::default();
    let writing = system_over(
        CustomStore::builder()
            .eventstore(events.clone())
            .snapshot(snapshots.clone())
            .build(),
    )
    .await;
    let id = AggregateId::new("proxy-snapshot-restart");
    {
        let proxy = writing.spawn_aggregate::<Counter>(id.clone()).await;
        for _ in 0..5 {
            proxy.ask(Increment).await.expect("ask must succeed");
        }
        let saved = proxy.snapshot().await.expect("snapshot must succeed");
        assert_eq!(saved, 5, "the snapshot must be taken at sequence 5");
        proxy.ask(Increment).await.expect("ask 6");
        proxy.ask(Increment).await.expect("ask 7");
    }

    // When
    let recorder = ReplayRecorder::over(events);
    let restarted = system_over(
        CustomStore::builder()
            .eventstore(recorder.clone())
            .snapshot(snapshots)
            .build(),
    )
    .await;
    let recovered = restarted.spawn_aggregate::<Counter>(id.clone()).await;
    let count = recovered.exec(GetCount).await.expect("exec must succeed");

    // Then
    assert_eq!(count, 7, "snapshot (5) + 2 events replayed after it = 7");
    assert_eq!(
        recorder.replayed(id.as_str()),
        vec![6, 7],
        "the restarted activation must replay only what followed the snapshot; \
         replaying from 1 would mean the snapshot was not found or not used"
    );
}

/// Given a Counter that was told to increment 3 times, When `snapshot()` is
/// called right after, Then the snapshot is taken after all three: the
/// activation's mailbox applies them before it reaches the snapshot.
#[tokio::test]
async fn snapshot_captures_the_state_after_commands_sent_before_it() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("proxy-snapshot-after-tells");
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    for _ in 0..3 {
        proxy.tell(Increment).await.expect("tell must be accepted");
    }

    // When
    let saved = proxy.snapshot().await.expect("snapshot must succeed");

    // Then
    assert_eq!(
        saved, 3,
        "the snapshot must be taken after the three told commands were applied"
    );
    let snapshot = latest_snapshot(&system, &id).await;
    assert_eq!(snapshot.sequence, 3);
    assert_eq!(
        decode_snapshot(&snapshot.payload),
        3,
        "the saved payload must be the count after the three told commands"
    );
}

/// Given a Counter that was spawned and has appended nothing, When
/// `snapshot()` is called, Then sequence 0 is saved like any other: it answers
/// 0 and the persistor reads back a snapshot at sequence 0.
#[tokio::test]
async fn snapshot_of_an_activation_that_appended_nothing_is_saved_at_sequence_zero() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("proxy-snapshot-sequence-zero");
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;

    // When
    let saved = proxy
        .snapshot()
        .await
        .expect("snapshot at sequence 0 must not be refused");

    // Then
    assert_eq!(saved, 0);
    let snapshot = latest_snapshot(&system, &id).await;
    assert_eq!(
        snapshot.sequence, 0,
        "sequence 0 must be saved, not skipped"
    );
    assert_eq!(decode_snapshot(&snapshot.payload), 0);
}

// Failures

/// Given an aggregate whose snapshot codec cannot write, When `snapshot()` is
/// called, Then it fails as an encode failure and nothing is saved.
#[tokio::test]
async fn a_snapshot_the_codec_cannot_write_fails_as_encode_and_saves_nothing() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("proxy-snapshot-encode-failure");
    let proxy = system.spawn_aggregate::<Unencodable>(id.clone()).await;

    // When
    let err = proxy
        .snapshot()
        .await
        .expect_err("a snapshot the codec cannot write must not be reported as saved");

    // Then
    assert!(
        matches!(err, SnapshotSaveError::Encode(CodecError::Encode(_))),
        "expected an encode failure, got {err:?}"
    );
    let stored = system
        .snapshot_persistor()
        .await
        .load_latest(id)
        .await
        .expect("loading the snapshot must succeed");
    assert!(
        stored.is_none(),
        "a snapshot that failed to encode must not reach the store"
    );
}

/// Given a snapshot store that refuses every save, When `snapshot()` is
/// called, Then it fails as a persist failure carrying the store's error.
#[tokio::test]
async fn a_snapshot_the_store_refuses_fails_as_persist() {
    // Given
    let system = system_over(
        CustomStore::builder()
            .eventstore(InMemoryEventStore::default())
            .snapshot(RefusingSnapshotStore)
            .build(),
    )
    .await;
    let id = AggregateId::new("proxy-snapshot-persist-failure");
    let proxy = system.spawn_aggregate::<Counter>(id).await;

    // When
    let err = proxy
        .snapshot()
        .await
        .expect_err("a snapshot the store refused must not be reported as saved");

    // Then
    assert!(
        matches!(err, SnapshotSaveError::Persist(SnapshotError::Backend(_))),
        "expected the store's refusal as a persist failure, got {err:?}"
    );
}

/// Given a reference whose activation stopped on a failed replay, When
/// `snapshot()` is called through it, Then it fails as a send failure; and the
/// reference lets go of the stopped activation, so the next `snapshot()`
/// reaches a fresh one and succeeds.
#[tokio::test]
async fn an_unreachable_activation_fails_as_send_and_the_next_snapshot_reaches_a_fresh_one() {
    // Given
    let system = system_over(
        CustomStore::builder()
            .eventstore(FailFirstLoadStore::new())
            .snapshot(InMemorySnapshotStore::default())
            .build(),
    )
    .await;
    let id = AggregateId::new("proxy-snapshot-send-failure");
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    // Awaited on a reference of its own, so `proxy` still names the stopped
    // activation when it dispatches below.
    await_stopped(&system.aggregate_proxy::<Counter>(id)).await;

    // When
    let err = proxy
        .snapshot()
        .await
        .expect_err("a stopped activation must not answer a snapshot");

    // Then
    assert!(
        matches!(err, SnapshotSaveError::Send(_)),
        "expected the stopped activation to be unreachable, got {err:?}"
    );
    let saved = proxy.snapshot().await.expect(
        "after a send failure the reference must resolve a live activation \
         rather than dispatch into the stopped one again",
    );
    assert_eq!(saved, 0, "the fresh activation replayed an empty stream");
}
