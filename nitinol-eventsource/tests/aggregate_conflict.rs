// How an aggregate activation answers a store OCC rejection (C-1, C-2).
//
// C-1: a conflict on a non-genesis sequence means this activation is no longer
//      the stream's only writer, so it stops.
// C-2: a conflict on the genesis sequence means the aggregate has already been
//      created.  That is a normal response, and the activation lives on.
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

use nitinol_eventsource::error::EffectExecutionError;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Context, Decider, Effect,
    Event, ExecError, Query,
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

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
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
            AskError::Effect(EffectExecutionError::Append(AppendError::SequenceConflict(
                _
            )))
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

// C-2: genesis conflict

/// C-2: a conflict on the genesis sequence is the store's "already created"
/// answer, so the dispatch succeeds and reports that nothing new was written.
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
    let events = redelivered
        .ask(Increment)
        .await
        .expect("a genesis conflict means 'already created', not a failure");

    // Then
    assert!(
        events.is_empty(),
        "nothing was persisted, so no event may be reported as persisted: {events:?}"
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
        .expect("a genesis conflict must not fail the dispatch");

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
        .expect("first genesis conflict");

    // When
    let events = redelivered
        .ask(Increment)
        .await
        .expect("second genesis conflict");

    // Then
    assert!(
        events.is_empty(),
        "the repeated command must still write nothing: {events:?}"
    );
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1],
        "a writer refused at the genesis sequence must never reach sequence 2"
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
