// A non-genesis `SequenceConflict` leaves a stable observation point behind.
//
// C-1 stops an overtaken writer and the reference above it resolves a fresh
// activation, so the self-repair is invisible from the outside: nothing tells an
// operator how often it is happening.  The rate matters, because it is what
// decides whether a single-activation optimisation is worth its cost and whether
// an aggregate boundary is drawing too many writers — and both are judgements
// that were deferred to measurement.
//
// So the conflict is written to the one channel that needs no receiver: the
// crate's own tracing records.  What this file fixes is that the record is
// findable — that its target and its name are stable values an operator can
// filter on rather than a file path and a line number — and that it names only
// the conflict it is about.  Nothing else that ends an append may fire it: not
// a genesis collision, which is a normal answer rather than a lost race; not a
// store failure; not a refusal the decider reached before any append existed.
//
// What the record cannot say is *why* the stream was overtaken.  A second
// activation of the same aggregate and a stray writer against the same store
// produce the identical conflict, so this is a count of lost races, not a
// diagnosis of their cause.
//
// The subscriber that captures the records is installed globally, which can
// happen once per process, so this file holds exactly one test and divides it
// into phases.  Each phase uses its own aggregate id and the records are
// attributed by that id, so a phase never reads another's.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};

use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Query,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;

// The observation point's published identity
//
// Written out here rather than read from the crate, because what these tests
// fix is the pair of values themselves: an operator's filter is written against
// them, so a rename is a breaking change and has to fail.  Reading a constant
// out of the implementation would rename in step with it and fix nothing.

const OCC_TARGET: &str = "nitinol_eventsource::occ";
const OCC_EVENT_NAME: &str = "non_genesis_sequence_conflict";

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("occ-observed"), TypeName::new("Incremented"));

    type Codec = JsonCodec;
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

// Fixtures: commands and query

struct Increment;
struct IncrementIfLessThan(u64);
struct GetCount;

#[derive(Debug, thiserror::Error)]
#[error("counter already at {0}")]
struct AtMaxError(u64);

impl Decider<Increment> for Counter {
    type Output = u64;
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, u64, Self::Rejection> {
        Decision::persist(vec![Incremented]).output(self.value + 1)
    }
}

/// Refuses without producing facts, so the dispatch ends before any append
/// exists to conflict with.
impl Decider<IncrementIfLessThan> for Counter {
    type Output = u64;
    type Rejection = AtMaxError;

    fn decide(&self, cmd: IncrementIfLessThan) -> Decision<Incremented, u64, AtMaxError> {
        if self.value >= cmd.0 {
            return Decision::reject(AtMaxError(self.value));
        }
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

// Fixtures: a store that answers every append with a backend failure
//
// Loads are delegated, so an activation against it replays whatever was seeded
// before it was wrapped and then fails its append for a reason that is not a
// lost race.

struct AppendFailsWithBackendError {
    inner: InMemoryEventStore,
}

impl AppendFailsWithBackendError {
    /// Seeds the genesis event through the inner store, so a writer activated
    /// against the wrapper replays past the genesis sequence and the append it
    /// fails is a non-genesis one.
    async fn seeded_with_genesis(id: &AggregateId) -> Self {
        let inner = InMemoryEventStore::default();
        let payload = <JsonCodec as Codec<Incremented>>::encode(&Incremented)
            .expect("the fixture event must encode");
        inner
            .append(
                id.as_str(),
                vec![AppendingEvent {
                    sequence: 1,
                    event_type: Incremented::EVENT_TYPE,
                    payload,
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("seeding the genesis event must succeed");
        Self { inner }
    }
}

#[async_trait]
impl EventStore for AppendFailsWithBackendError {
    async fn append(
        &self,
        _key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        Err(AppendError::Backend(
            "simulated backend failure on append".into(),
        ))
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Fixtures: capturing the crate's tracing records

/// One captured record, reduced to what this test judges: the two values that
/// make it findable, and the fields it carries.
#[derive(Clone, Debug)]
struct Reported {
    target: String,
    name: String,
    fields: Vec<(String, String)>,
}

impl Reported {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(recorded, _)| recorded == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether this is the observation point rather than some other record the
    /// crate emitted.
    fn is_the_observation_point(&self) -> bool {
        self.target == OCC_TARGET && self.name == OCC_EVENT_NAME
    }

    /// Whether this record says it is about `aggregate_id`.
    ///
    /// Compared by containment because a field's captured rendering depends on
    /// how the value was recorded, and which of those the implementation picks
    /// is not part of the contract; that the id is named is.
    fn names(&self, aggregate_id: &str) -> bool {
        self.field("aggregate_id")
            .is_some_and(|recorded| recorded.contains(aggregate_id))
    }
}

/// Keeps each field of a record separately, so a value can be judged on its own
/// rather than by searching one flattened line.
///
/// Only `record_debug` is implemented because every other `Visit` method falls
/// back to it, so a value recorded as a string, a number or a display value all
/// arrive here.
#[derive(Default)]
struct FieldPairs(Vec<(String, String)>);

impl Visit for FieldPairs {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

struct Recorder {
    reported: Arc<Mutex<Vec<Reported>>>,
}

/// Why the capture buffer's lock cannot be poisoned: it guards a push and a
/// read of a `Vec`, and no holder can panic.
const RECORD_LOCK: &str = "the capture buffer guards only a push and a read, so no holder panics";

impl Subscriber for Recorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = FieldPairs::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        self.reported.lock().expect(RECORD_LOCK).push(Reported {
            target: metadata.target().to_owned(),
            name: metadata.name().to_owned(),
            fields: fields.0,
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Every captured observation point that names `aggregate_id`.
fn observations_of(reported: &Mutex<Vec<Reported>>, aggregate_id: &str) -> Vec<Reported> {
    reported
        .lock()
        .expect(RECORD_LOCK)
        .iter()
        .filter(|record| record.is_the_observation_point() && record.names(aggregate_id))
        .cloned()
        .collect()
}

// Helpers

/// Activate one writer for `id` on `store` through the raw props path.
///
/// The raw path spawns per call, which is what lets a phase put two writers on
/// one stream.
async fn spawn_writer(
    ps: &ProcessSystem,
    id: &AggregateId,
    store: &Arc<dyn EventStore>,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id.clone(), Arc::clone(store))
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
        "the writer must be parked after {expected} event(s) before the phase proceeds"
    );
}

// The test
//
// Every phase reads the buffer only after the `ask` it is about has returned.
// The observation is made inside the dispatch, before the answer travels back,
// so by then the record either exists or never will — no phase waits or polls.

/// A non-genesis conflict leaves exactly one findable record naming the
/// aggregate and the sequence it lost; nothing else that ends an append leaves
/// one.
#[tokio::test]
async fn a_non_genesis_conflict_is_observed_once_and_nothing_else_is() {
    let reported = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(Recorder {
        reported: Arc::clone(&reported),
    })
    .expect("this test binary holds one test, so nothing else installs a subscriber");

    let ps = ProcessSystem::new().await;

    // Phase 1: a lost race is observed, once
    //
    // Both writers work the same stream under the same id, so a record
    // attributed to that id can only have come from the conflict: the winner's
    // two successful appends would otherwise be counted here too.
    {
        // Given: a writer that replayed to sequence 1, and a winner that then
        // takes sequence 2 out from under it.
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let id = AggregateId::new("occ-observed-non-genesis");

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
        assert!(
            matches!(
                err,
                AskError::Persist(PersistError::Append(AppendError::SequenceConflict(_)))
            ),
            "the phase must produce a non-genesis conflict, got {err:?}"
        );

        // Then
        let observed = observations_of(&reported, id.as_str());
        assert_eq!(
            observed.len(),
            1,
            "a lost race must leave exactly one record under target {OCC_TARGET:?} named \
             {OCC_EVENT_NAME:?}: a filter written against those two values is the whole of how \
             an operator counts these, so a record under the default target or the default \
             `event file:line` name is not findable, and one per conflict is what makes the \
             count a rate; got {observed:?}"
        );
        assert_eq!(
            observed[0].field("sequence"),
            Some("2"),
            "the record must name the sequence the append lost, so repeated conflicts on one \
             aggregate stay distinguishable and a non-genesis conflict is recognisable as such; \
             got {observed:?}"
        );
    }

    // Phase 2: a genesis collision is not a lost race
    //
    // Both writers exist before anything is stored, so both address the genesis
    // sequence and the loser is told the aggregate already exists.  That is a
    // normal answer to a redelivered creation; counting it would inflate the
    // rate with events that are not races at all.
    {
        // Given
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let id = AggregateId::new("occ-observed-genesis");

        let creator = spawn_writer(&ps, &id, &store).await;
        let redelivered = spawn_writer(&ps, &id, &store).await;
        pin_replayed_state(&redelivered, 0).await;

        creator.ask(Increment).await.expect("genesis append");

        // When
        let err = redelivered
            .ask(Increment)
            .await
            .expect_err("a creation that collides with an existing aggregate has no answer");
        assert!(
            matches!(err, AskError::AlreadyCreated),
            "the phase must produce a genesis collision, got {err:?}"
        );

        // Then
        let observed = observations_of(&reported, id.as_str());
        assert!(
            observed.is_empty(),
            "a genesis collision is the expected shape of a redelivered creation, not a writer \
             losing its stream, so it must leave no observation behind, got {observed:?}"
        );
    }

    // Phase 3: a store failure is not a lost race
    //
    // The append fails at a non-genesis sequence, so the only thing separating
    // it from phase 1 is which `AppendError` the store returned.
    {
        // Given
        let id = AggregateId::new("occ-observed-backend-failure");
        let store: Arc<dyn EventStore> =
            Arc::new(AppendFailsWithBackendError::seeded_with_genesis(&id).await);

        let writer = spawn_writer(&ps, &id, &store).await;
        pin_replayed_state(&writer, 1).await;

        // When
        let err = writer
            .ask(Increment)
            .await
            .expect_err("the store must refuse the append");
        assert!(
            matches!(
                err,
                AskError::Persist(PersistError::Append(AppendError::Backend(_)))
            ),
            "the phase must produce a backend failure rather than a conflict, got {err:?}"
        );

        // Then
        let observed = observations_of(&reported, id.as_str());
        assert!(
            observed.is_empty(),
            "a store that failed to answer says nothing about who owns the stream, so counting \
             it would read as contention that never happened, got {observed:?}"
        );
    }

    // Phase 4: a refusal is not a lost race
    //
    // The decider refuses before any facts exist, so no append is attempted at
    // all.
    {
        // Given
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let id = AggregateId::new("occ-observed-rejection");

        let writer = spawn_writer(&ps, &id, &store).await;

        // When
        let err = writer
            .ask(IncrementIfLessThan(0))
            .await
            .expect_err("the decider must refuse the command");
        assert!(
            matches!(err, AskError::Rejection(AtMaxError(0))),
            "the phase must produce a domain refusal, got {err:?}"
        );

        // Then
        let observed = observations_of(&reported, id.as_str());
        assert!(
            observed.is_empty(),
            "a command the decider refused never reached the store, so it cannot have lost a \
             race and must not be counted as one, got {observed:?}"
        );
    }
}
