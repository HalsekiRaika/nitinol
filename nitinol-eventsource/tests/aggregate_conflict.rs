// How an aggregate activation answers a store OCC rejection (C-1, C-2).
//
// C-1: a conflict on a non-genesis sequence means this activation is no longer
//      the stream's only writer, so it stops.
// C-2: a conflict on the genesis sequence means the aggregate has already been
//      created.  No decision was reached, so the caller is told exactly that
//      and is handed no answer; the activation lives on.
//
// Both cases are produced without a test double.  The raw `AggregateProps` path
// activates per call, so two writers can be put on one stream and the in-memory
// store rejects whichever one falls behind.  Which of the two branches
// a writer takes is decided by when it was activated: a writer spawned before
// anything was stored still addresses the genesis sequence, one spawned after
// replays past it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    ExecError, Query, TellError,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("conflict"), TypeName::new("Incremented"));
}

// Fixtures: aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: command and query

struct Increment;
struct GetCount;

/// The answer the command asks for is the counter's new value, so an
/// interpreter that reported a collision as a success would have to invent a
/// number nobody decided.
impl Decider<Increment> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Fixtures: codec

#[derive(Default)]
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// Helpers

/// Activate one writer for `id` on `store` through the raw props path.
///
/// The raw path spawns per call, which is what lets a test put two writers on
/// one stream.
async fn spawn_writer(
    ps: &ProcessSystem,
    id: &AggregateId,
    store: &Arc<dyn EventStore>,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id.clone(), Arc::clone(store))
        .with_codec(Arc::new(JsonCodec))
        .spawn(ps)
        .await
}

/// Wait until `proxy`'s activation has finished replaying, and confirm it
/// stopped at `expected` events.
///
/// A spawn returns before `on_start` has replayed, so which sequence a writer
/// addresses next is otherwise decided by scheduling.  An activation answers
/// user messages only after that replay, so one query round-trip both waits for
/// it and reports where it landed — which is what decides whether the writer's
/// next append is a genesis append.
async fn pin_replayed_state(proxy: &AggregateProxy<Counter>, expected: u64) {
    let count = proxy
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        count, expected,
        "the writer must be parked after {expected} event(s) before the test proceeds"
    );
}

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &Arc<dyn EventStore>, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.iter().map(|event| event.sequence).collect()
}

/// Block until `proxy`'s activation stops answering.
///
/// The stop is carried out by the activation's own loop after the failing
/// dispatch has already returned, so the caller can observe it only by querying
/// again.  A writer that never stops keeps answering and fails on the timeout.
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
    .expect("the writer that lost the stream must stop")
}

// C-1: non-genesis conflict

/// C-1: a writer whose non-genesis append is rejected stops.
///
/// The loser is activated after the genesis event exists, so it replays to
/// sequence 1 and the append it loses is sequence 2 — not a creation.
#[tokio::test]
async fn non_genesis_conflict_stops_the_losing_writer() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-non-genesis");

    let winner = spawn_writer(&ps, &id, &store).await;
    winner.ask(Increment).await.expect("genesis append");

    let loser = spawn_writer(&ps, &id, &store).await;
    pin_replayed_state(&loser, 1).await;

    winner
        .ask(Increment)
        .await
        .expect("the winner takes sequence 2");

    // When
    let err = loser
        .ask(Increment)
        .await
        .expect_err("a writer that fell behind must be rejected by the store");

    // Then
    assert!(
        matches!(
            err,
            AskError::Persist(PersistError::Append(AppendError::SequenceConflict(_)))
        ),
        "the store's OCC rejection must reach the caller unchanged, got {err:?}"
    );
    await_stopped(&loser).await;
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2],
        "a rejected append must leave the stream as the winner wrote it, and the \
         loser must not reload and retry"
    );
}

/// C-1: the stop is reported to a `tell` sender too, as a refused delivery.
///
/// `tell` keeps no channel for a decision's verdict, so what it owes a sender is
/// narrower: that `Ok(())` means the command was taken for delivery.  Once C-1
/// has stopped the writer, that is no longer true of it, and a `tell` still
/// answering `Ok(())` would leave the caller believing a command was queued for
/// an activation that will never read it — the silent loss the told path exists
/// to avoid.  The failure is what a caller acts on: it names a dispatch that did
/// not happen, so the command can be re-issued through a reference that resolves
/// a live writer.
#[tokio::test]
async fn a_tell_to_a_writer_stopped_by_a_conflict_is_refused() {
    // Given: the loser of the same race C-1 stops
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-non-genesis-tell");

    let winner = spawn_writer(&ps, &id, &store).await;
    winner.ask(Increment).await.expect("genesis append");

    let loser = spawn_writer(&ps, &id, &store).await;
    pin_replayed_state(&loser, 1).await;

    winner
        .ask(Increment)
        .await
        .expect("the winner takes sequence 2");
    loser
        .ask(Increment)
        .await
        .expect_err("the writer that fell behind must lose the stream");
    await_stopped(&loser).await;

    // When: a command is told to the writer that just stopped
    let outcome = loser.tell(Increment).await;

    // Then
    assert!(
        matches!(outcome, Err(TellError::Send(_))),
        "a command told to a stopped writer must be reported as undelivered rather than \
         accepted for a delivery that cannot happen, got {outcome:?}"
    );
}

// C-2: genesis conflict

/// C-2: a conflict on the genesis sequence is the store's "already created"
/// answer, and that is what the caller is told.
///
/// No decision was reached — the events never landed — so there is no output to
/// deliver.  Reporting the collision as a success would force the interpreter
/// to invent one, and the caller would believe it created what someone else
/// did.
#[tokio::test]
async fn genesis_conflict_is_answered_as_already_created() {
    // Given: both writers exist before anything is stored, so both address the
    // genesis sequence.
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-genesis-answer");

    let creator = spawn_writer(&ps, &id, &store).await;
    let redelivered = spawn_writer(&ps, &id, &store).await;
    pin_replayed_state(&redelivered, 0).await;

    creator.ask(Increment).await.expect("genesis append");

    // When
    let err = redelivered
        .ask(Increment)
        .await
        .expect_err("a creation that collides with an existing aggregate has no answer to give");

    // Then
    assert!(
        matches!(err, AskError::AlreadyCreated),
        "the collision must be reported as such rather than dressed up as a rejection or a \
         store failure, got {err:?}"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "the already-created stream must be left exactly as the creator wrote it"
    );
}

/// C-2: the writer that saw a genesis conflict stays alive, and must not apply
/// an event the store refused.
#[tokio::test]
async fn genesis_conflict_leaves_the_writer_alive_and_unchanged() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-genesis-alive");

    let creator = spawn_writer(&ps, &id, &store).await;
    let redelivered = spawn_writer(&ps, &id, &store).await;
    pin_replayed_state(&redelivered, 0).await;

    creator.ask(Increment).await.expect("genesis append");

    // When
    redelivered
        .ask(Increment)
        .await
        .expect_err("a genesis conflict is reported, not answered");

    // Then
    let count = redelivered
        .exec(GetCount)
        .await
        .expect("the writer must survive a genesis conflict");
    assert_eq!(
        count, 0,
        "an event the store refused must not be applied to the aggregate state"
    );
}

/// C-2: a refused genesis append must not move the writer's sequence forward.
///
/// A writer that advanced on a refused append would go on writing into a stream
/// it never replayed — the very state C-1 exists to end.  Repeating the command
/// is what makes that visible: it must be refused the same way, not accepted at
/// the next sequence.
#[tokio::test]
async fn genesis_conflict_does_not_advance_the_writer_sequence() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-genesis-sequence");

    let creator = spawn_writer(&ps, &id, &store).await;
    let redelivered = spawn_writer(&ps, &id, &store).await;
    pin_replayed_state(&redelivered, 0).await;

    creator.ask(Increment).await.expect("genesis append");
    redelivered
        .ask(Increment)
        .await
        .expect_err("first genesis conflict");

    // When
    let err = redelivered
        .ask(Increment)
        .await
        .expect_err("second genesis conflict");

    // Then
    assert!(
        matches!(err, AskError::AlreadyCreated),
        "the repeated command must be refused the same way rather than accepted at the next \
         sequence, got {err:?}"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "a writer refused at the genesis sequence must never reach sequence 2"
    );
}

/// C-2: an already-created answer is not the death of an activation, so a
/// reference must keep dispatching to the one it already resolved.
///
/// A reference drops the activation it cached only when that activation can no
/// longer be reached — which is what C-1 makes true of an overtaken writer, and
/// what C-2 explicitly does not make true here.  Dropping it would cost a
/// resolve nobody asked for, and the difference is observable: an activation
/// resolved afresh replays the creator's event, while the one that saw the
/// collision never did.
#[tokio::test]
async fn genesis_conflict_keeps_the_reference_on_its_activation() {
    // Given: a resolved reference parked at the genesis sequence
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("conflict-genesis-reference");

    let resolved = system.spawn_aggregate::<Counter>(id.clone()).await;
    pin_replayed_state(&resolved, 0).await;

    // And: someone else creates the aggregate first
    let creator = AggregateProps::<Counter>::new(id.clone(), Arc::clone(&store))
        .with_codec(system.codec::<Incremented>())
        .spawn(system.process_system())
        .await;
    creator.ask(Increment).await.expect("genesis append");

    // When
    let err = resolved
        .ask(Increment)
        .await
        .expect_err("the resolved reference must be told the aggregate already exists");
    assert!(
        matches!(err, AskError::AlreadyCreated),
        "the setup must produce a genesis collision, got {err:?}"
    );

    // Then
    let count = resolved
        .exec(GetCount)
        .await
        .expect("the reference must still reach an activation");
    assert_eq!(
        count, 0,
        "the reference must still hold the activation that saw the collision: one resolved \
         again would have replayed the creator's event and answered 1"
    );
}

// C-2 must not apply to an activation that never finished replaying

/// A store whose `load` fails exactly once, then delegates to an in-memory
/// backend for every call after — including its own retries by a later
/// activation.  Models a transient replay failure (backend hiccup) rather
/// than a permanently broken store.
struct FailFirstLoadStore {
    inner: InMemoryEventStore,
    fail_next_load: AtomicBool,
}

impl FailFirstLoadStore {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            fail_next_load: AtomicBool::new(true),
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

/// C-2's "already created" answer assumes the activation replayed far enough to
/// know that `sequence == 0` means it addressed the genesis sequence honestly.
/// An activation whose replay itself failed also has `sequence == 0`, but for
/// an unrelated reason: it never read the stream at all. Answering a later
/// genesis-sequence conflict as "already created" in that case would hide a
/// stale activation behind the same success the redelivery case gets.
///
/// The activation must stop on the failed replay instead, the same way C-1
/// stops an overtaken activation — so it never gets to decide anything from
/// state it never actually reached.
#[tokio::test]
async fn replay_failure_does_not_masquerade_as_a_genesis_conflict() {
    // Given: the writer's own replay fails before anything is stored.
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(FailFirstLoadStore::new());
    let id = AggregateId::new("conflict-genesis-replay-failure");

    let unreplayed = spawn_writer(&ps, &id, &store).await;
    await_stopped(&unreplayed).await;

    // And: another writer later creates the aggregate for real.
    let creator = spawn_writer(&ps, &id, &store).await;
    creator.ask(Increment).await.expect("genesis append");

    // Then: the writer that failed to replay must already be gone — it must
    // not still be alive to answer a later genesis conflict as "already
    // created" from state it never replayed.
    let err = unreplayed
        .ask(Increment)
        .await
        .expect_err("an activation whose replay failed must not answer commands");
    assert!(
        matches!(err, AskError::Send(_)),
        "expected the stopped activation to be unreachable, got {err:?}"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "only the real creator's genesis event may exist"
    );
}

/// The decode-failure branch of replay must behave like the load and stream
/// failure branches next to it: an activation that cannot decode an existing
/// event has not reached the state it would decide from, so it must stop
/// rather than continue with `sequence == 0` and let a later append conflict
/// on the genesis sequence be answered as C-2's "already created" from state
/// it never actually replayed.
#[tokio::test]
async fn replay_decode_failure_does_not_masquerade_as_a_genesis_conflict() {
    // Given: the stream already holds an event this writer's codec cannot
    // decode.
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("conflict-genesis-decode-failure");
    store
        .append(
            id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: Incremented::EVENT_TYPE,
                payload: Bytes::from_static(b"not valid json"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding the undecodable event must succeed");

    // When: a writer is activated against that stream.
    let unreplayed = spawn_writer(&ps, &id, &store).await;
    await_stopped(&unreplayed).await;

    // Then: it must already be gone rather than still answering as though it
    // replayed to `sequence == 0` and can treat the next append conflict on
    // the genesis sequence as a redelivered creation.
    let err = unreplayed.ask(Increment).await.expect_err(
        "an activation that could not decode an existing event must not answer commands",
    );
    assert!(
        matches!(err, AskError::Send(_)),
        "expected the stopped activation to be unreachable, got {err:?}"
    );
}
