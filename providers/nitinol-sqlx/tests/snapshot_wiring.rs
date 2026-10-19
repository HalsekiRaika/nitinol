//! Every slot of the Postgres store under one `EventSourceSystem`: an
//! aggregate that recovers from its newest snapshot, restarted over the same
//! database.
//!
//! ```gherkin
//! Feature: nitinol-sqlx PostgresStore, every slot wired
//!
//!   Scenario: an aggregate recovers from its snapshot and replays what followed
//!     Given a system built over PersistenceProvider::new(PostgresStore::new(pool))
//!     When an aggregate declaring Recover = Latest is spawned on it, is carried
//!          to sequence 8, and the system's snapshot persistor saves a snapshot
//!          at sequence 5
//!     And a second system over the same pool starts the aggregate again
//!     Then it compiles
//!     And the aggregate recovers from that snapshot and replays only the
//!         events above its position
//! ```
//!
//! What this holds up that the store's own clauses cannot: whether an
//! aggregate's restored state and the events replayed above it add up to where
//! that aggregate had got to is a property of the store *and* the process
//! driving it. A store shown to keep every snapshot clause has not thereby been
//! shown to be driven correctly.
//!
//! # Why compiling is half the clause
//!
//! An aggregate declaring `Recover = Latest` requires the system it is spawned
//! on to hold a snapshot slot, so a store without one is refused where the
//! spawn is compiled rather than when a load is first attempted. This file
//! spawning at all is that half.
//!
//! # Why the saved state is not the number of events
//!
//! An activation that cannot read its snapshot replays from the first event
//! instead, and says so only in a log line. A snapshot whose state was the
//! count of events written would make the two outcomes the same number, and the
//! clause would pass without the snapshot having been read at all. The saved
//! state is therefore a count the events alone could never produce, and what is
//! read back is that state plus the events above the snapshot's position and
//! nothing else — a full replay lands on 8, and a recovery that ignored the
//! events above the position lands on 100.
//!
//! # Why the restart is a second system
//!
//! One system resolves one activation per identity, so asking the same system
//! again would reach the state already resident in memory rather than recover
//! anything. A restart is a second system, on a process system of its own, over
//! a store assembled again from the same pool.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use bytes::Bytes;
use linkme::distributed_slice;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, AggregateProps, AggregateProxy, Decider, Decision, Event, Latest, Query,
    Snapshotable,
};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::{
    AggregateId, EventRegistryEntry, EventType, Family, PersistedSnapshot, TypeName, EVENT_REGISTRY,
};
use nitinol_runtime::ProcessSystem;
use nitinol_sqlx::PostgresStore;
use sqlx::PgPool;

/// How far the aggregate is carried before a snapshot is taken of it.
const EVENTS: u64 = 8;

/// The position the saved snapshot names.
const SNAPSHOT_AT: u64 = 5;

/// The state the saved snapshot carries at that position.
///
/// Not the number of events: see this file's own notes on why.
const SNAPSHOT_STATE: u64 = 100;

/// What the restarted aggregate must answer with: the saved state, plus one
/// application for each event above the position the snapshot names.
const RECOVERED: u64 = SNAPSHOT_STATE + (EVENTS - SNAPSHOT_AT);

const INCREMENTED: EventType =
    EventType::new(Family::new("sqlx.snapshot"), TypeName::new("Incremented"));

/// What makes `INCREMENTED` restorable: a store resolves what its binary
/// linked, so an event type a clause journals has to be declared to the
/// collection the same way a domain event is.
#[distributed_slice(EVENT_REGISTRY)]
static INCREMENTED_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::INCREMENTED"), &[INCREMENTED]);

#[derive(Clone)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = INCREMENTED;

    type Codec = NoPayload;
}

/// The event says all it has to say by existing, so there is nothing for a
/// codec to carry.
struct NoPayload;

impl Codec<Incremented> for NoPayload {
    type Error = std::convert::Infallible;

    fn encode(_: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

/// An aggregate whose state is one number, so that what it recovered from is
/// readable as one answer.
#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;
    /// The declaration that requires the system to hold a snapshot slot.
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for Counter {
    type Snapshot = u64;

    type Codec = BigEndianCount;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

struct BigEndianCount;

#[derive(Debug, thiserror::Error)]
#[error("a count is eight bytes, and this snapshot carried {0}")]
struct NotACount(usize);

impl Codec<u64> for BigEndianCount {
    type Error = NotACount;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        payload
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| NotACount(payload.len()))
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

/// A system over `pool`, on a process system of its own.
///
/// The store is assembled here rather than handed in, because a restart is a
/// store assembled again: a handle carried over from the first system would
/// make the clause about one assembly used twice rather than about the rows.
async fn system_over(pool: &PgPool) -> EventSourceSystem<PostgresStore> {
    let store = PostgresStore::new(pool.clone())
        .expect("the event types this test binary linked claim distinct canonical paths");
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// Start `id` on `system`.
///
/// `Counter` declares `Recover = Latest`, so this compiles only where the
/// system's store holds a snapshot slot.
async fn counter(
    system: &EventSourceSystem<PostgresStore>,
    id: AggregateId,
) -> AggregateProxy<Counter> {
    AggregateProps::<Counter>::new(id).spawn(system).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aggregate_recovers_from_its_snapshot_and_replays_only_what_followed() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let counted = AggregateId::new("sqlx.snapshot.wiring");

    let writing = system_over(&pool).await;
    {
        let proxy = counter(&writing, counted.clone()).await;
        for _ in 0..EVENTS {
            proxy
                .ask(Increment)
                .await
                .expect("an aggregate journalling to a database this run reached takes a command");
        }
    }

    writing
        .snapshot_persistor()
        .await
        .save(PersistedSnapshot {
            aggregate_id: counted.clone(),
            sequence: SNAPSHOT_AT,
            payload: Bytes::from(SNAPSHOT_STATE.to_be_bytes().to_vec()),
            created_at: fixture::fixed_instant(),
        })
        .await
        .expect("the one persistor the system distributes must take a save");

    let restarted = system_over(&pool).await;
    let count = counter(&restarted, counted)
        .await
        .exec(GetCount)
        .await
        .expect("an aggregate that has recovered must answer a query");

    assert_eq!(
        count, RECOVERED,
        "the restarted aggregate must recover the state the snapshot named and then apply only \
         the events above that position: a count of {EVENTS} is a replay from the first event \
         with the snapshot never read, and a count of {SNAPSHOT_STATE} is a recovery that never \
         replayed what followed it"
    );
}
