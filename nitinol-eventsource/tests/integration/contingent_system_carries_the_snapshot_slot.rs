//! The contingent system keeps the journal of the deployment it was taken from
//! and whether that deployment holds a snapshot slot, and an aggregate resolved
//! through it acts on that value.
//!
//! ```gherkin
//!   Scenario: the contingent system carries the snapshot slot the store holds
//!     Given a system over a store that holds a snapshot slot
//!     When it is turned into a contingent system, and that is cloned twice
//!     Then each of them answers with that store's snapshot slot
//!
//!   Scenario: the contingent system carries the journal the store holds
//!     Given a system over a store that holds a journal
//!     When it is turned into a contingent system, and that is cloned twice
//!     Then a fact appended through any of their journals is what the store's
//!          own journal loads
//!
//!   Scenario: a store without a snapshot slot carries none
//!     Given a system over a store that holds no snapshot slot
//!     When it is turned into a contingent system, and that is cloned
//!     Then neither answers with a snapshot store
//!
//!   Scenario: a `Latest` aggregate resolved through the contingent system
//!             recovers
//!     Given a snapshot saved through the system's snapshot persistor
//!     When the aggregate is resolved through the contingent system
//!     Then it starts from that snapshot
//!
//!   Scenario: a `Latest` aggregate on a deployment without a slot is refused
//!     Given a system over a store that holds no snapshot slot
//!     And a stream the aggregate would replay
//!     When a `Latest` aggregate is resolved through its contingent system,
//!          lazily or eagerly
//!     Then every ask, tell, query and snapshot through the reference fails with
//!          an activation failure, every time it is tried
//!     And ask and tell report that failure as permanent
//!     And the stream is not replayed in place of the missing snapshot
//!
//!   Scenario: a `Never` aggregate needs no snapshot slot
//!     Given a system over a store that holds no snapshot slot
//!     When a `Never` aggregate is resolved through its contingent system,
//!          lazily or eagerly
//!     Then it accepts the command and appends to its stream
//! ```
//!
//! On a typed system, a `Latest` aggregate over a store without the slot is a
//! compile error, fixed by the `compile_fail` doctests on the spawn entry
//! points. The contingent system has no type that could say so, so there the
//! same mistake has to be visible when the program runs. Each failure clause
//! below repeats its dispatch for that reason. A refusal that came back once
//! and then turned into a transient send failure would tell a caller to retry
//! something that cannot succeed.
//!
//! The stream the refused aggregate would replay is seeded before it is
//! resolved. If the missing slot were treated as "no snapshot yet", the
//! activation would replay those facts and accept the command, and the stream
//! would grow. What the clause reads is that it did not.
//!
//! The slot and journal clauses write through what the contingent system
//! answers with and read back through the store the system was built on. A
//! contingent system that built a journal or an album of its own would still
//! answer, but nothing written through it would reach the deployment.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration contingent_system_carries_the_snapshot_slot::`.

use std::time::Duration;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::{ContingentSystem, EventSourceSystem};
use nitinol_eventsource::{
    Aggregate, AggregateProxy, AskError, Decider, Decision, Event, ExecError, Latest, Never, Query,
    Retryability, SnapshotSaveError, Snapshotable, TellError,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Store};
use nitinol_persistence::store::{Album, EventStore, InMemoryEventStore, InMemoryStore, Journal};
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
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("contingent.system"),
        TypeName::new("Incremented"),
    );

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

// Fixture: the aggregate that recovers from its newest snapshot.

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

// Fixture: the aggregate that replays its whole stream.

#[derive(Default)]
struct Tally {
    value: u64,
}

impl Aggregate for Tally {
    type Event = Incremented;
    type Recover = Never;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Decider<Increment> for Tally {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Increment) -> Decision<Incremented, (), Self::Rejection> {
        Decision::persist(vec![Incremented]).output(())
    }
}

impl Query<GetCount> for Tally {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

// Helpers

/// How long an eager spawn may take before it counts as hung.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times each refusal clause dispatches.
const ATTEMPTS: u32 = 2;

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

/// Save a snapshot of `aggregate` at `sequence` through `album`.
async fn save_at(album: &Album, aggregate: &AggregateId, sequence: u64) {
    album
        .save(a_snapshot(aggregate, sequence, sequence))
        .await
        .expect("saving through an album succeeds");
}

/// The sequence `album` holds for `aggregate`, if any.
async fn album_sequence(album: &Album, aggregate: &AggregateId) -> Option<u64> {
    album
        .load_latest(aggregate)
        .await
        .expect("loading from an album succeeds")
        .map(|found| found.sequence)
}

/// Append one increment to `key` at `sequence` through `journal`.
async fn append_through(journal: &Journal, key: &AggregateId, sequence: u64) {
    journal
        .append(
            key.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: Incremented::EVENT_TYPE,
                payload: Bytes::from_static(IncrementedCodec::TAG),
                occurred_at: Timestamp::now(),
            }],
        )
        .await
        .expect("appending through a journal succeeds");
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

/// The same, through a journal.
async fn journal_sequences(journal: &Journal, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = journal
        .load(LoadQuery::by_stream(id))
        .await
        .expect("loading a stream of a journal succeeds")
        .try_collect()
        .await
        .expect("collecting a stream of a journal succeeds");
    loaded.iter().map(|event| event.sequence).collect()
}

/// Ask `proxy` [`ATTEMPTS`] times and require the permanent activation failure
/// every time.
async fn every_ask_is_refused_permanently(proxy: &AggregateProxy<Counter>, entry_point: &str) {
    for attempt in 1..=ATTEMPTS {
        let err = proxy.ask(Increment).await.expect_err(
            "a `Latest` aggregate on a deployment without a snapshot slot must not \
             accept a command",
        );
        assert!(
            matches!(err, AskError::Activation(_)),
            "{entry_point}, ask attempt {attempt}: the failure must say the \
             aggregate could not be activated, not that a send failed; got {err:?}"
        );
        assert_eq!(
            err.retryability(),
            Retryability::Permanent,
            "{entry_point}, ask attempt {attempt}: the deployment will not grow a \
             snapshot slot, so re-issuing the command cannot succeed"
        );
    }
}

/// Tell `proxy` [`ATTEMPTS`] times and require the permanent activation failure
/// every time.
async fn every_tell_is_refused_permanently(proxy: &AggregateProxy<Counter>, entry_point: &str) {
    for attempt in 1..=ATTEMPTS {
        let err = proxy.tell(Increment).await.expect_err(
            "a told command has nowhere to go when the aggregate cannot be \
             activated, so it must not be reported as accepted",
        );
        assert!(
            matches!(err, TellError::Activation(_)),
            "{entry_point}, tell attempt {attempt}: the failure must say the \
             aggregate could not be activated, not that a send failed; got {err:?}"
        );
        assert_eq!(
            err.retryability(),
            Retryability::Permanent,
            "{entry_point}, tell attempt {attempt}: re-telling cannot succeed on \
             this deployment"
        );
    }
}

// The slot and the journal through the contingent system

/// The contingent system answers with the snapshot slot of the store it was
/// taken from, and so do its clone and the clone of that clone.
///
/// A clone is how a contingent system travels — into a saga instance, into a
/// delivery partition — so a clone that dropped the slot would refuse the
/// aggregate one hand-over further down.
#[tokio::test]
async fn the_contingent_system_and_its_clones_answer_with_the_stores_snapshot_slot() {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let through_contingent = AggregateId::new("contingent-slot-itself");
    let through_clone = AggregateId::new("contingent-slot-clone");
    let through_clone_of_clone = AggregateId::new("contingent-slot-clone-of-clone");

    // When
    let contingent: ContingentSystem = system.to_contingent();
    let clone = contingent.clone();
    let clone_of_clone = clone.clone();
    save_at(
        &contingent
            .snapshot_store()
            .expect("a store that holds a snapshot slot keeps it in the contingent system"),
        &through_contingent,
        3,
    )
    .await;
    save_at(
        &clone
            .snapshot_store()
            .expect("a clone of the contingent system keeps the slot it carries"),
        &through_clone,
        5,
    )
    .await;
    save_at(
        &clone_of_clone
            .snapshot_store()
            .expect("and so does a clone of that clone"),
        &through_clone_of_clone,
        7,
    )
    .await;

    // Then
    let typed = system.persistence().snapshot_store();
    assert_eq!(
        album_sequence(&typed, &through_contingent).await,
        Some(3),
        "the slot the contingent system answers with is the snapshot slot of the \
         store the system was built on"
    );
    assert_eq!(
        album_sequence(&typed, &through_clone).await,
        Some(5),
        "its clone answers with that same slot"
    );
    assert_eq!(
        album_sequence(&typed, &through_clone_of_clone).await,
        Some(7),
        "and the clone of that clone does too"
    );
}

/// The contingent system answers with the journal of the store it was taken
/// from, and so do its clone and the clone of that clone.
#[tokio::test]
async fn the_contingent_system_and_its_clones_answer_with_the_stores_journal() {
    // Given
    let store = InMemoryStore::default();
    let system = system_over(store.clone()).await;
    let stream = AggregateId::new("contingent-journal");

    // When
    let contingent = system.to_contingent();
    let clone = contingent.clone();
    let clone_of_clone = clone.clone();
    append_through(&contingent.event_store(), &stream, 1).await;
    append_through(&clone.event_store(), &stream, 2).await;
    append_through(&clone_of_clone.event_store(), &stream, 3).await;

    // Then
    assert_eq!(
        journal_sequences(&store.events(), &stream).await,
        vec![1, 2, 3],
        "the journal the contingent system and its clones answer with is the \
         journal of the store the system was built on"
    );
}

/// The contingent system of a system whose store holds no snapshot slot
/// answers with none, and so does its clone.
#[tokio::test]
async fn a_store_without_a_snapshot_slot_leaves_the_contingent_system_without_one() {
    // Given
    let system = system_over(journal_only(&InMemoryEventStore::default())).await;

    // When
    let contingent = system.to_contingent();
    let clone = contingent.clone();

    // Then
    assert!(
        contingent.snapshot_store().is_none(),
        "a store that holds no snapshot slot has none to carry, and the \
         contingent system must not supply one of its own"
    );
    assert!(
        clone.snapshot_store().is_none(),
        "a clone carries the absence as faithfully as the slot"
    );
}

// A deployment that holds the slot

/// A `Latest` aggregate resolved through the contingent system starts from the
/// snapshot the system's own persistor saved.
///
/// The snapshot holds 40 and the stream holds nothing, so 40 can only have come
/// from the snapshot: an activation that replayed the stream instead would
/// start from 0.
#[tokio::test]
async fn a_latest_aggregate_resolved_through_the_contingent_system_recovers_from_the_systems_snapshot(
) {
    // Given
    let system = system_over(InMemoryStore::default()).await;
    let id = AggregateId::new("contingent-latest-recovers");
    system
        .snapshot_persistor()
        .await
        .save(a_snapshot(&id, 0, 40))
        .await
        .expect("saving through the system's snapshot persistor succeeds");

    // When
    let proxy = system.to_contingent().aggregate_proxy::<Counter>(id);
    proxy.ask(Increment).await.expect(
        "a `Latest` aggregate on a deployment that holds a snapshot slot is \
         resolved through the contingent system like any other",
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

/// Asking and telling a `Latest` aggregate through a reference the contingent
/// system built lazily fails with a permanent activation failure on every
/// attempt, and the stream it would have replayed is left as it was.
#[tokio::test]
async fn a_lazily_resolved_latest_aggregate_is_refused_permanently_without_a_snapshot_slot() {
    // Given: a stream with facts a replay would apply
    let events = InMemoryEventStore::default();
    let system = system_over(journal_only(&events)).await;
    let id = AggregateId::new("contingent-no-slot-lazy");
    seed_increments(&events, &id, 2).await;

    // When
    let proxy = system
        .to_contingent()
        .aggregate_proxy::<Counter>(id.clone());

    // Then
    every_ask_is_refused_permanently(&proxy, "aggregate_proxy").await;
    every_tell_is_refused_permanently(&proxy, "aggregate_proxy").await;

    // And: nothing replayed the stream in place of the missing snapshot
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1, 2],
        "an activation that replayed the stream for want of a snapshot would have \
         accepted a command and appended to it"
    );
}

/// The eager entry point returns, and the reference it hands back answers with
/// the same permanent activation failure on every attempt, leaving the stream
/// as it was.
///
/// An eager spawn that panicked, hung, or started an activation that then died
/// would each show up here: as a failed test, as the timeout, or as a send
/// failure in place of the activation failure.
#[tokio::test]
async fn an_eagerly_spawned_latest_aggregate_is_refused_permanently_without_a_snapshot_slot() {
    // Given: a stream with facts a replay would apply
    let events = InMemoryEventStore::default();
    let system = system_over(journal_only(&events)).await;
    let id = AggregateId::new("contingent-no-slot-eager");
    seed_increments(&events, &id, 2).await;
    let contingent = system.to_contingent();

    // When
    let proxy = tokio::time::timeout(
        SPAWN_TIMEOUT,
        contingent.spawn_aggregate::<Counter>(id.clone()),
    )
    .await
    .expect("an eager spawn of an aggregate that cannot be activated must return");

    // Then
    every_ask_is_refused_permanently(&proxy, "spawn_aggregate").await;
    every_tell_is_refused_permanently(&proxy, "spawn_aggregate").await;

    // And: nothing replayed the stream in place of the missing snapshot
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1, 2],
        "an activation that replayed the stream for want of a snapshot would have \
         accepted a command and appended to it"
    );
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
        .to_contingent()
        .aggregate_proxy::<Counter>(AggregateId::new("contingent-no-slot-other"));

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

// A `Never` aggregate on a deployment that does not hold the slot

/// A `Never` aggregate asks nothing of the snapshot slot, so the contingent
/// system of a deployment without one resolves it through either entry point,
/// and the commands land on its stream.
///
/// A `Never` aggregate refused for want of a slot it never needed would fail
/// either dispatch below and leave the stream short.
#[tokio::test]
async fn a_never_aggregate_is_resolved_through_the_contingent_system_without_a_snapshot_slot() {
    // Given
    let events = InMemoryEventStore::default();
    let system = system_over(journal_only(&events)).await;
    let contingent = system.to_contingent();
    let id = AggregateId::new("contingent-never-no-slot");

    // When
    contingent
        .aggregate_proxy::<Tally>(id.clone())
        .ask(Increment)
        .await
        .expect("a `Never` aggregate needs no snapshot slot, so the lazy reference accepts");
    let spawned = tokio::time::timeout(
        SPAWN_TIMEOUT,
        contingent.spawn_aggregate::<Tally>(id.clone()),
    )
    .await
    .expect("an eager spawn of a `Never` aggregate must return");
    spawned
        .ask(Increment)
        .await
        .expect("and so does the eagerly spawned one");

    // Then
    assert_eq!(
        stored_sequences(&events, &id).await,
        vec![1, 2],
        "both commands were decided by an activation and appended to the stream"
    );
    assert_eq!(
        spawned
            .exec(GetCount)
            .await
            .expect("the query must be answered"),
        2,
        "the activation applied both increments"
    );
}
