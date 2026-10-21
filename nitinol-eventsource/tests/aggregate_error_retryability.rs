// Retryability of AggregateProxy errors (R-5).
//
// R-5 requires the caller to be able to tell, from the type rather than from an
// error message, whether retrying can help.  A dispatch that failed because the
// activation lost the stream and stopped is transient: the reference re-resolves
// and the command can be re-issued.  A dispatch the decider refused is permanent:
// the same command against the same state is refused again.
//
// Every error asserted on here is produced by a real dispatch, so the mapping is
// checked against the errors the framework actually builds.

use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Never, Query, Retryability,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, Journal};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("retryability"), TypeName::new("Incremented"));

    type Codec = JsonCodec;
}

// Fixtures: aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: commands and query

struct Increment;
struct GetCount;

/// Command the decider refuses once the counter has reached `0`, i.e. always.
struct IncrementUntil(u64);

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

impl Decider<IncrementUntil> for Counter {
    type Output = u64;
    type Rejection = AtMaxError;

    fn decide(&self, cmd: IncrementUntil) -> Decision<Incremented, u64, AtMaxError> {
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

// Helpers

/// A system distributing `store`, with an aggregate registry of its own.
///
/// One registry resolves an identity to one activation, so a second writer of
/// one stream is a second system over the same store.
async fn system_over(store: &Journal) -> EventSourceSystem<Journal> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store.clone()))
        .build()
}

/// Activate one writer for `id` on `system`.
async fn spawn_writer(
    system: &EventSourceSystem<Journal>,
    id: &AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id.clone())
        .spawn(system)
        .await
}

/// Put two writers on one stream and return the error the one that fell behind
/// receives for a non-genesis append, together with a reference still
/// addressing the writer that lost.
///
/// The reference the conflict is reported to treats losing a stream as an
/// invalidation and resolves a fresh activation on its next dispatch, so it
/// cannot be used to reach the activation that stopped.  A second reference to
/// the same identity on the same system is taken before the race and holds the
/// activation it first reached, which is how a dispatch to a stopped activation
/// is reached at all.
async fn non_genesis_conflict(
    id: &AggregateId,
    store: &Journal,
) -> (AggregateProxy<Counter>, AskError<std::convert::Infallible>) {
    let winning = system_over(store).await;
    let losing = system_over(store).await;

    let winner = spawn_writer(&winning, id).await;
    winner.ask(Increment).await.expect("genesis append");

    let loser = spawn_writer(&losing, id).await;
    // A spawn returns before `on_start` has replayed, and an activation answers
    // user messages only afterwards — so this query pins the loser at sequence 1
    // and makes the append it is about to lose a non-genesis one.
    let replayed = loser
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        replayed, 1,
        "the loser must be parked after the genesis event before the winner moves on"
    );

    let stopped = losing.aggregate_proxy::<Counter>(id.clone());
    let addressed = stopped
        .exec(GetCount)
        .await
        .expect("the second reference must reach the activation that is about to lose");
    assert_eq!(
        addressed, 1,
        "the second reference must be addressing the writer that is about to lose, not a \
         second activation of it"
    );

    winner
        .ask(Increment)
        .await
        .expect("the winner takes sequence 2");

    let err = loser
        .ask(Increment)
        .await
        .expect_err("a writer that fell behind must be rejected by the store");
    (stopped, err)
}

// R-5-TRANSIENT

/// The OCC rejection that triggers stop-on-conflict is transient: the stream
/// moved on, and a re-resolved activation can carry the command.
#[tokio::test]
async fn sequence_conflict_is_transient() {
    // Given
    let store = Journal::from(InMemoryEventStore::default());
    let id = AggregateId::new("retryability-conflict");

    // When
    let (_loser, err) = non_genesis_conflict(&id, &store).await;

    // Then
    assert!(
        matches!(
            err,
            AskError::Persist(PersistError::Append(AppendError::SequenceConflict(_)))
        ),
        "the setup must produce a sequence conflict, got {err:?}"
    );
    assert_eq!(
        err.retryability(),
        Retryability::Transient,
        "losing the stream to another writer must be retryable"
    );
}

/// A dispatch to an activation that has already stopped is transient for the
/// same reason: it says nothing about the command, only about where it was sent.
#[tokio::test]
async fn dispatch_to_a_stopped_activation_is_transient() {
    // Given
    let store = Journal::from(InMemoryEventStore::default());
    let id = AggregateId::new("retryability-stopped");
    let (loser, _conflict) = non_genesis_conflict(&id, &store).await;

    // When: keep dispatching to the reference that still addresses the writer
    // that lost, until the stop lands
    let err = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match loser.ask(Increment).await {
                Err(e @ AskError::Send(_)) => return e,
                Err(_) => tokio::task::yield_now().await,
                Ok(output) => {
                    panic!("a writer that lost the stream must not answer with {output:?}")
                }
            }
        }
    })
    .await
    .expect("the writer that lost the stream must stop");

    // Then
    assert_eq!(
        err.retryability(),
        Retryability::Transient,
        "a send failure carries no verdict on the command, so it must be retryable"
    );
}

// R-5-PERMANENT

/// A decider's refusal is a verdict on the command against the current state,
/// so re-issuing it cannot help.
#[tokio::test]
async fn decider_rejection_is_permanent() {
    // Given
    let store = Journal::from(InMemoryEventStore::default());
    let system = system_over(&store).await;
    let proxy = AggregateProps::<Counter>::new(AggregateId::new("retryability-rejection"))
        .spawn(&system)
        .await;

    // When
    let err = proxy
        .ask(IncrementUntil(0))
        .await
        .expect_err("the decider must refuse at the limit");

    // Then
    assert!(
        matches!(err, AskError::Rejection(_)),
        "the setup must produce a decider rejection, got {err:?}"
    );
    assert_eq!(
        err.retryability(),
        Retryability::Permanent,
        "a business refusal must not invite a retry"
    );
}

/// Being told the aggregate already exists is a verdict on this command against
/// this stream, not a report about where it was sent: re-issuing the same
/// creation against the same stream collides with the same first write again.
#[tokio::test]
async fn already_created_is_permanent() {
    // Given: both writers address the genesis sequence
    let store = Journal::from(InMemoryEventStore::default());
    let creating_system = system_over(&store).await;
    let redelivering_system = system_over(&store).await;
    let id = AggregateId::new("retryability-already-created");

    let creator = spawn_writer(&creating_system, &id).await;
    let redelivered = spawn_writer(&redelivering_system, &id).await;
    let replayed = redelivered
        .exec(GetCount)
        .await
        .expect("the activation must answer once it has replayed");
    assert_eq!(
        replayed, 0,
        "the redelivered creation must still be addressing the genesis sequence"
    );

    creator.ask(Increment).await.expect("genesis append");

    // When
    let err = redelivered
        .ask(Increment)
        .await
        .expect_err("a creation that collides with an existing aggregate has no answer to give");

    // Then
    assert!(
        matches!(err, AskError::AlreadyCreated),
        "the setup must produce a genesis collision, got {err:?}"
    );
    assert_eq!(
        err.retryability(),
        Retryability::Permanent,
        "an aggregate that already exists will exist on the next attempt too"
    );
}
