//! The erased handle keeps whether the deployment holds a snapshot slot, and
//! a `Recover = Latest` aggregate resolved through it acts on that value.
//!
//! ```gherkin
//!   Scenario: erasing keeps the snapshot slot the store holds
//!     Given a system over a store that holds a snapshot slot
//!     When its store type is erased, once and then again
//!     Then each erased handle carries that store's snapshot slot
//!
//!   Scenario: erasing a store without a snapshot slot carries none
//!     Given a system over a store that holds no snapshot slot
//!     When its store type is erased
//!     Then the erased handle carries no snapshot slot
//!
//!   Scenario: a `Latest` aggregate resolved through the erased handle recovers
//!     Given a snapshot saved through the system's snapshot persistor
//!     When the aggregate is resolved through the erased handle
//!     Then it starts from that snapshot
//!
//!   Scenario: a `Latest` aggregate on a deployment without a slot is refused
//!     Given a system over a store that holds no snapshot slot
//!     When a `Latest` aggregate is resolved through its erased handle, lazily
//!          or eagerly
//!     Then every ask, tell, query and snapshot through the reference fails with
//!          an activation failure, every time it is tried
//!     And ask and tell report that failure as permanent
//!     And the stream is not replayed in place of the missing snapshot
//! ```
//!
//! On a typed system, a `Latest` aggregate over a store without the slot is a
//! compile error, fixed by the `compile_fail` doctests on the spawn entry
//! points. The erased handle has no type that could say so, so there the same
//! mistake has to be visible when the program runs. Each failure clause below
//! repeats its dispatch for that reason. A refusal that came back once and then
//! turned into a transient send failure would tell a caller to retry something
//! that cannot succeed.
//!
//! The stream the refused aggregate would replay is seeded before it is
//! resolved. If the missing slot were treated as "no snapshot yet", the
//! activation would replay those facts and accept the command, and the stream
//! would grow. What the clause reads is that it did not.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration erased_handle_carries_the_snapshot_slot::`.

use std::time::Duration;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AskError, Decider, Decision, Event, ExecError, Latest, Query,
    Retryability, SnapshotSaveError, Snapshotable, TellError,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Store};
use nitinol_persistence::store::{Album, EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, PersistedSnapshot, TypeName,
};
use nitinol_runtime::ProcessSystem;

// Fixture: the event and its codec.

#[derive(Clone, Debug)]
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
        EventType::new(Family::new("erased.handle"), TypeName::new("Incremented"));

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

// Helpers

/// How long an eager spawn may take before it counts as hung.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

async fn system_over<St: Store>(store: St) -> EventSourceSystem<St> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// A store holding `events` as its journal and no other slot.
fn journal_only(events: &InMemoryEventStore) -> CustomStore {
    CustomStore::builder().eventstore(events.clone()).build()
}

fn a_snapshot(aggregate_id: &AggregateId, sequence: u64, value: u64) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: <CounterSnapshotCodec as Codec<u64>>::encode(&value)
            .expect("the fixture snapshot codec writes every value"),
        created_at: Timestamp::now(),
    }
}

/// The sequence `album` holds for `aggregate`, if any.
async fn album_sequence(album: &Album, aggregate: &AggregateId) -> Option<u64> {
    album
        .load_latest(aggregate)
        .await
        .expect("loading from an album succeeds")
        .map(|found| found.sequence)
}

/// Append `count` increments to `id`'s stream through the backend handle.
async fn seed_increments(events: &InMemoryEventStore, id: &AggregateId, count: u64) {
    let facts = (1..=count)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type: Incremented::EVENT_TYPE,
            payload: Bytes::from_static(IncrementedCodec::TAG),
            occurred_at: Timestamp::now(),
        })
        .collect();
    events
        .append(id.as_str(), facts)
        .await
        .expect("seeding a stream of an in-memory store succeeds");
}

/// The stream sequences stored under `id`, in ascending order.
async fn stored_sequences(events: &InMemoryEventStore, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = events
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of an in-memory store succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of an in-memory store succeeds");
    loaded.iter().map(|event| event.sequence).collect()
}

// The slot through the erasure

/// The erased handle carries the snapshot slot of the store it was erased from,
/// and erasing that handle again carries it too.
///
/// Erasing an erased handle is what an entry point written against the erased
/// type does to the handle it is given, so a second erasure that dropped the
/// slot would refuse the aggregate one call further down.
#[tokio::test]
async fn erasing_a_store_with_a_snapshot_slot_keeps_that_slot_through_every_erasure() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let through_erased = AggregateId::new("erased-handle-slot-once");
    let through_erased_twice = AggregateId::new("erased-handle-slot-twice");

    // When
    let erased = system.erase_store();
    let erased_twice = erased.erase_store();
    erased
        .persistence()
        .store()
        .snapshot_slot()
        .expect("erasing a store that holds a snapshot slot keeps that slot")
        .save(a_snapshot(&through_erased, 3, 3))
        .await
        .expect("saving through the erased handle's slot succeeds");
    erased_twice
        .persistence()
        .store()
        .snapshot_slot()
        .expect("erasing an erased handle keeps the slot it carries")
        .save(a_snapshot(&through_erased_twice, 5, 5))
        .await
        .expect("saving through the twice-erased handle's slot succeeds");

    // Then
    let typed = system.persistence().snapshot_store();
    assert_eq!(
        album_sequence(&typed, &through_erased).await,
        Some(3),
        "the slot the erased handle carries is the snapshot slot of the store \
         the system was built on"
    );
    assert_eq!(
        album_sequence(&typed, &through_erased_twice).await,
        Some(5),
        "and erasing that handle again carries the same slot"
    );
}

/// The erased handle of a system whose store holds no snapshot slot carries
/// none.
#[tokio::test]
async fn erasing_a_store_without_a_snapshot_slot_carries_none() {
    // Given
    let system = system_over(journal_only(&InMemoryEventStore::default())).await;

    // When
    let erased = system.erase_store();

    // Then
    assert!(
        erased.persistence().store().snapshot_slot().is_none(),
        "a store that holds no snapshot slot has none to carry through the \
         erasure, and the erased handle must not supply one of its own"
    );
}

// A deployment that holds the slot

/// A `Latest` aggregate resolved through the erased handle starts from the
/// snapshot the system's own persistor saved.
///
/// The snapshot holds 40 and the stream holds nothing, so 40 can only have come
/// from the snapshot: an activation that replayed the stream instead would
/// start from 0.
#[tokio::test]
async fn a_latest_aggregate_resolved_through_the_erased_handle_recovers_from_the_systems_snapshot()
{
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("erased-handle-latest-recovers");
    system
        .snapshot_persistor()
        .await
        .save(a_snapshot(&id, 0, 40))
        .await
        .expect("saving through the system's snapshot persistor succeeds");

    // When
    let proxy = system.erase_store().aggregate_proxy::<Counter>(id);
    proxy.ask(Increment).await.expect(
        "a `Latest` aggregate on a deployment that holds a snapshot slot is \
         resolved through the erased handle like any other",
    );

    // Then
    assert_eq!(
        proxy
            .exec(GetCount)
            .await
            .expect("the query must be answered"),
        41,
        "the activation started from the snapshot (40) and applied the one \
         command it was asked"
    );
}

// A deployment that does not hold the slot

/// Asking a `Latest` aggregate through the erased handle of a deployment
/// without a snapshot slot fails with an activation failure classified as
/// permanent, on every attempt, and the stream it would have replayed is left
/// as it was.
#[tokio::test]
async fn asking_through_the_erased_handle_fails_permanently_without_a_snapshot_slot() {
    // Given: a stream with facts a replay would apply
    let events = InMemoryEventStore::default();
    let system = system_over(journal_only(&events)).await;
    let id = AggregateId::new("erased-handle-no-slot-ask");
    seed_increments(&events, &id, 2).await;
    let proxy = system.erase_store().aggregate_proxy::<Counter>(id.clone());

    for attempt in 1..=2 {
        // When
        let err = proxy.ask(Increment).await.expect_err(
            "a `Latest` aggregate on a deployment without a snapshot slot must \
             not accept a command",
        );

        // Then
        assert!(
            matches!(err, AskError::Activation(_)),
            "attempt {attempt}: the failure must say the aggregate could not be \
             activated, not that a send failed; got {err:?}"
        );
        assert_eq!(
            err.retryability(),
            Retryability::Permanent,
            "attempt {attempt}: the deployment will not grow a snapshot slot, so \
             re-issuing the command cannot succeed"
        );
    }

    // And: nothing replayed the stream in place of the missing snapshot
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1, 2],
        "an activation that replayed the stream for want of a snapshot would have \
         accepted the command and appended to it"
    );
}

/// Telling a `Latest` aggregate through the erased handle of a deployment
/// without a snapshot slot fails with the same permanent activation failure,
/// rather than being accepted for delivery to nothing.
#[tokio::test]
async fn telling_through_the_erased_handle_fails_permanently_without_a_snapshot_slot() {
    // Given
    let events = InMemoryEventStore::default();
    let system = system_over(journal_only(&events)).await;
    let id = AggregateId::new("erased-handle-no-slot-tell");
    let proxy = system.erase_store().aggregate_proxy::<Counter>(id.clone());

    for attempt in 1..=2 {
        // When
        let err = proxy.tell(Increment).await.expect_err(
            "a told command has nowhere to go when the aggregate cannot be \
             activated, so it must not be reported as accepted",
        );

        // Then
        assert!(
            matches!(err, TellError::Activation(_)),
            "attempt {attempt}: the failure must say the aggregate could not be \
             activated, not that a send failed; got {err:?}"
        );
        assert_eq!(
            err.retryability(),
            Retryability::Permanent,
            "attempt {attempt}: re-telling cannot succeed on this deployment"
        );
    }
    assert!(
        stored_sequences(&events, &id).await.is_empty(),
        "nothing was appended for a command that was refused"
    );
}

/// The entry points that activate eagerly return, and the reference they hand
/// back answers with the same permanent activation failure.
///
/// An eager spawn that panicked, hung, or started an activation that then died
/// would each show up here: as a failed test, as the timeout, or as a send
/// failure in place of the activation failure.
#[tokio::test]
async fn an_eager_spawn_through_the_erased_handle_answers_with_the_same_permanent_failure() {
    // Given
    let system = system_over(journal_only(&InMemoryEventStore::default())).await;
    let erased = system.erase_store();

    // When: the system-first eager entry point
    let spawned = tokio::time::timeout(
        SPAWN_TIMEOUT,
        erased.spawn_aggregate::<Counter>(AggregateId::new("erased-handle-eager-system")),
    )
    .await
    .expect("an eager spawn of an aggregate that cannot be activated must return");

    // Then
    let err = spawned
        .ask(Increment)
        .await
        .expect_err("the spawned reference must not accept a command");
    assert!(
        matches!(err, AskError::Activation(_)),
        "the reference `spawn_aggregate` hands back must report the activation \
         failure; got {err:?}"
    );
    assert_eq!(err.retryability(), Retryability::Permanent);

    // When: the props-first eager entry point
    let spawned = tokio::time::timeout(
        SPAWN_TIMEOUT,
        AggregateProps::<Counter>::new(AggregateId::new("erased-handle-eager-props"))
            .spawn(&erased),
    )
    .await
    .expect("an eager spawn from props must return too");

    // Then
    let err = spawned
        .tell(Increment)
        .await
        .expect_err("the spawned reference must not accept a told command");
    assert!(
        matches!(err, TellError::Activation(_)),
        "the reference `AggregateProps::spawn` hands back must report the \
         activation failure; got {err:?}"
    );
    assert_eq!(err.retryability(), Retryability::Permanent);
}

/// A query and a snapshot through a reference that cannot be activated report
/// the activation failure as well, rather than a send failure.
///
/// A snapshot request on such a reference must never reach an activation,
/// because none exists that could capture state. A send failure here would also
/// tell the caller the reference may resolve a live activation next time,
/// which on this deployment it never will.
#[tokio::test]
async fn a_query_and_a_snapshot_through_an_unresolvable_reference_report_the_activation_failure() {
    // Given
    let system = system_over(journal_only(&InMemoryEventStore::default())).await;
    let proxy = system
        .erase_store()
        .aggregate_proxy::<Counter>(AggregateId::new("erased-handle-no-slot-other"));

    // When
    let queried = proxy
        .exec(GetCount)
        .await
        .expect_err("there is no activation to answer the query");
    let snapshotted = proxy
        .snapshot()
        .await
        .expect_err("there is no activation to capture a snapshot of");

    // Then
    assert!(
        matches!(queried, ExecError::Activation(_)),
        "the query must report the activation failure; got {queried:?}"
    );
    assert!(
        matches!(snapshotted, SnapshotSaveError::Activation(_)),
        "the snapshot must report the activation failure; got {snapshotted:?}"
    );
}
