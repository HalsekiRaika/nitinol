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

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::error::PersistError;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProps, AggregateProxy, AskError, Decider, Decision, Event,
    Query, Retryability,
};
use nitinol_persistence::error::AppendError;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("retryability"), TypeName::new("Incremented"));
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

/// Activate one writer for `id` on `store` through the raw props path, which
/// activates per call.
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

/// Put two writers on one stream and return the error the one that fell behind
/// receives for a non-genesis append.
async fn non_genesis_conflict(
    ps: &ProcessSystem,
    id: &AggregateId,
    store: &Arc<dyn EventStore>,
) -> (AggregateProxy<Counter>, AskError<std::convert::Infallible>) {
    let winner = spawn_writer(ps, id, store).await;
    winner.ask(Increment).await.expect("genesis append");

    let loser = spawn_writer(ps, id, store).await;
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

    winner
        .ask(Increment)
        .await
        .expect("the winner takes sequence 2");

    let err = loser
        .ask(Increment)
        .await
        .expect_err("a writer that fell behind must be rejected by the store");
    (loser, err)
}

// R-5-TRANSIENT

/// The OCC rejection that triggers stop-on-conflict is transient: the stream
/// moved on, and a re-resolved activation can carry the command.
#[tokio::test]
async fn sequence_conflict_is_transient() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("retryability-conflict");

    // When
    let (_loser, err) = non_genesis_conflict(&ps, &id, &store).await;

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
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("retryability-stopped");
    let (loser, _conflict) = non_genesis_conflict(&ps, &id, &store).await;

    // When: keep dispatching to the pinned reference until the stop lands
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
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = AggregateProps::<Counter>::new(
        AggregateId::new("retryability-rejection"),
        Arc::clone(&store),
    )
    .with_codec(Arc::new(JsonCodec))
    .spawn(&ps)
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
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("retryability-already-created");

    let creator = spawn_writer(&ps, &id, &store).await;
    let redelivered = spawn_writer(&ps, &id, &store).await;
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
