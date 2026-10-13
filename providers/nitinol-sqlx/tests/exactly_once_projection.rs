//! `DeliveryMode::ExactlyOnce` over Postgres, as the acceptance criteria state
//! it.
//!
//! ```gherkin
//! Feature: OffsetStore (Postgres) and ExactlyOnce projection
//!
//!   Scenario: an incremental read model under redelivery
//!     Given a projector wired for ExactlyOnce with a TxProvider, and an
//!           incremental read model
//!     When the same event is redelivered
//!     Then the read model is applied to once and no more
//!     And the offset moved in the same transaction as the read-model
//!         update
//!
//!   Scenario: a rollback on a failure partway through
//!     Given a projector wired for ExactlyOnce with a TxProvider
//!     When project, or the offset save, fails
//!     Then the transaction is rolled back
//!     And the offset does not move, and the event is not treated as
//!         handled
//!
//!   Scenario: concurrent projectors kept from applying twice (the defence)
//!     Given two independent ProcessSystems over one projection_id and one
//!           database, both holding the same stale offset
//!     When both try to apply the same event
//!     Then the read model is updated once and no more
//!     And the one that lost is rolled back as an error that can be observed
//!
//!   Scenario: two starting where no offset has been written yet
//!     Given a projection_id with no offset record, and two projectors
//!           doing their first work at the same time
//!     Then the read model is updated once and no more, and the one that lost
//!          is rolled back as an error that can be observed
//! ```
//!
//! The read model is a counter, which is the shape that cannot absorb a second
//! application of one event: an overwrite would land on the same value twice
//! and say nothing. It is created by the clause rather than by this crate's
//! migrations, because a read model belongs to whoever projects into it.
//!
//! The `TxProvider` below is the one a user writes. This crate defines none —
//! see [`PoolTransactions`] — so the clauses here are also where the wiring the
//! README documents is put through a database.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use fixture::{committed, typed};
use linkme::distributed_slice;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    Event, ProjectionContext, ProjectionId, Projector, ProjectorProps, TxProvider,
};
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{EventStore, OffsetScope, OffsetStore};
use nitinol_persistence::{
    AggregateId, EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY,
};
use nitinol_runtime::process::{Process, ProcessProxy};
use nitinol_runtime::ProcessSystem;
use nitinol_sqlx::{OffsetConflict, PostgresEventStore, PostgresOffsetStore};
use sqlx::{PgPool, PgTransaction};
use tokio::sync::Barrier;

/// How long a clause waits for a projector to reach the state it is about.
///
/// The framework polls its subscription on a cadence of its own and every step
/// of an application crosses a socket, so what a clause waits on is the state
/// itself rather than a sleep long enough to have probably covered it. The
/// deadline is what turns a projection that never gets there into a failure
/// rather than a run that hangs.
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);
const SETTLE_POLL: Duration = Duration::from_millis(25);

/// How long the two projectors of a concurrency clause have to meet.
const RENDEZVOUS_DEADLINE: Duration = Duration::from_secs(30);

// The event a projection counts.

const INCREMENTED: EventType =
    EventType::new(Family::new("sqlx.offset"), TypeName::new("Incremented"));

/// What makes `INCREMENTED` restorable: a store resolves what its binary
/// linked, so an event type a clause appends has to be declared to the
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

// The user's half of an ExactlyOnce wiring.

/// The `TxProvider` a user writes to put this provider's offset store
/// under an `ExactlyOnce` projector.
///
/// `nitinol-sqlx` defines none, and the clauses here are what keeps that
/// honest: a transaction over a `PgPool` is that pool's own `begin`, `commit`
/// and `rollback`, and a provider that wrapped them would have to depend on
/// the interpreter crate that declares `TxProvider` — the wrong way round for
/// a provider. Nothing is left to get wrong silently:
/// `ProjectorProps::with_tx_provider` requires the offset store's `Tx` to
/// be this one's, so a pairing that would not share a transaction does not
/// compile.
struct PoolTransactions {
    pool: PgPool,
}

#[async_trait]
impl TxProvider for PoolTransactions {
    type Tx = PgTransaction<'static>;
    type Error = sqlx::Error;

    async fn begin(&self) -> Result<Self::Tx, Self::Error> {
        self.pool.begin().await
    }

    async fn commit(&self, tx: Self::Tx) -> Result<(), Self::Error> {
        tx.commit().await
    }

    async fn rollback(&self, tx: Self::Tx) {
        // The trait gives a rollback nowhere to report to, and there is nothing
        // left worth reporting: a rollback that could not be sent means the
        // connection is going away, and the transaction it held goes with it.
        let _ = tx.rollback().await;
    }
}

#[derive(Debug, thiserror::Error)]
enum ProjectionFailed {
    #[error("the read model update failed: {0}")]
    ReadModel(#[from] sqlx::Error),
    /// What the rollback clause needs: a `project` that has already written the
    /// read model inside the framework's transaction and then fails.
    #[error("intentional failure, so that a clause can watch the rollback")]
    Intentional,
    /// ExactlyOnce delivery owes `project` the transaction the offset save
    /// will ride. Without it the read-model update would land on a connection
    /// of its own, and "the offset and the read model move in one
    /// transaction" would be a claim nothing held up.
    #[error("ExactlyOnce delivery must hand project() the transaction to write in")]
    NoTransaction,
}

/// The increment every clause here watches.
struct CountingProjector {
    fail: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
    /// Where the two projectors of a concurrency clause meet.
    rendezvous: Option<Arc<Barrier>>,
}

#[async_trait]
impl Projector<Incremented, PgTransaction<'static>> for CountingProjector {
    type Error = ProjectionFailed;

    async fn project(
        &mut self,
        _event: Incremented,
        ctx: &mut ProjectionContext<'_, PgTransaction<'static>>,
    ) -> Result<(), Self::Error> {
        self.attempts.fetch_add(1, Ordering::SeqCst);

        // Only the first attempt waits. An event the losing projector could not
        // settle is redelivered, and a second wait would park it for a partner
        // that is not coming. The wait is *before* the read-model update so
        // that neither projector is already holding that row when the other
        // arrives — behind the update they would serialise there rather than
        // race for the offset.
        if let Some(rendezvous) = self.rendezvous.take() {
            tokio::time::timeout(RENDEZVOUS_DEADLINE, rendezvous.wait())
                .await
                .expect("both projectors of a concurrency clause must reach their first event");
        }

        let tx = ctx.tx().ok_or(ProjectionFailed::NoTransaction)?;
        sqlx::query("UPDATE applied_count SET applied = applied + 1 WHERE singleton")
            .execute(&mut **tx)
            .await?;

        if self.fail.load(Ordering::SeqCst) {
            return Err(ProjectionFailed::Intentional);
        }
        Ok(())
    }
}

/// `PostgresOffsetStore`, with the refusals it answered kept where a clause
/// can read them.
///
/// A refused save is answered by the framework — it rolls the transaction back
/// and redelivers — and the error goes no further than a log line. A clause
/// about the losing projector needs the error itself, so the store that
/// projector is given keeps one, and keeps it by the name a caller would use:
/// a conflict is counted only where [`OffsetConflict`] is reachable
/// through it.
struct ObservedOffsets {
    inner: PostgresOffsetStore,
    conflicts: Arc<AtomicUsize>,
}

#[async_trait]
impl OffsetStore for ObservedOffsets {
    type Tx = PgTransaction<'static>;

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        self.inner.load(scope).await
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        let outcome = self.inner.save(scope, sequence, tx).await;
        if let Err(OffsetError::Backend(ref cause)) = outcome {
            if cause.downcast_ref::<OffsetConflict>().is_some() {
                self.conflicts.fetch_add(1, Ordering::SeqCst);
            }
        }
        outcome
    }
}

fn observed(pool: PgPool, conflicts: &Arc<AtomicUsize>) -> Arc<ObservedOffsets> {
    Arc::new(ObservedOffsets {
        inner: PostgresOffsetStore::new(pool),
        conflicts: Arc::clone(conflicts),
    })
}

// What a clause reads and waits on.

/// The read model, created by the clause that projects into it.
async fn create_read_model(pool: &PgPool) {
    sqlx::query(
        r#"
        CREATE TABLE applied_count (
            singleton BOOLEAN PRIMARY KEY CHECK (singleton),
            applied   BIGINT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .expect("the schema this clause was handed must take a read model of its own");

    sqlx::query("INSERT INTO applied_count (singleton, applied) VALUES (TRUE, 0)")
        .execute(pool)
        .await
        .expect("the read model this clause created must take its starting row");
}

/// How many times the read model has been incremented, read on a connection of
/// its own so that what it reports is what has committed.
async fn applied(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT applied FROM applied_count WHERE singleton")
        .fetch_one(pool)
        .await
        .expect("the read model this clause created must answer")
}

async fn offset(store: &PostgresOffsetStore, projection_id: &ProjectionId) -> Option<u64> {
    let scope = OffsetScope::from(projection_id);
    store
        .load(&scope)
        .await
        .expect("a load against a database this run reached must answer")
}

/// Wait until the read model and the offset are both where `what` says, or
/// fail rather than hang.
async fn until_settled(
    what: &str,
    pool: &PgPool,
    offsets: &PostgresOffsetStore,
    projection_id: &ProjectionId,
    applied_count: i64,
    offset_at: Option<u64>,
) {
    let reached = async {
        loop {
            if applied(pool).await == applied_count
                && offset(offsets, projection_id).await == offset_at
            {
                return;
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    };
    tokio::time::timeout(SETTLE_DEADLINE, reached)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Wait until the counters between them reach `expected`, or fail rather than
/// hang.
///
/// A slice rather than one counter because which of two projectors loses a race
/// is not fixed, so what a clause can wait on is what the two saw between them.
async fn until_counted(what: &str, counters: &[&AtomicUsize], expected: usize) {
    let total = || {
        counters
            .iter()
            .map(|counter| counter.load(Ordering::SeqCst))
            .sum::<usize>()
    };
    let reached = async {
        while total() < expected {
            tokio::time::sleep(SETTLE_POLL).await;
        }
    };
    tokio::time::timeout(SETTLE_DEADLINE, reached)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Stop `proxy` and wait for the runtime to let go of it.
///
/// A clause that read the read model while a projector it thought it had
/// stopped was still applying events would be reading a value on its way
/// somewhere, so what it waits for is the process leaving the registry rather
/// than the stop having been sent.
async fn stopped<P: Process>(system: &ProcessSystem, proxy: &ProcessProxy<P>) {
    proxy
        .stop()
        .await
        .expect("a running projector must take a stop");
    let pid = proxy.pid();
    let gone = async {
        while system.lookup(pid).await.is_some() {
            tokio::time::sleep(SETTLE_POLL).await;
        }
    };
    tokio::time::timeout(SETTLE_DEADLINE, gone)
        .await
        .expect("a projector that was told to stop must leave the runtime's registry");
}

/// Acceptance: "an incremental read model under redelivery".
///
/// A projector restarted on the same `projection_id` is handed the offset
/// the first one left, so the event it already applied is not delivered to it
/// again. What makes that a claim about *this* store rather than about the
/// framework is that the offset crossed a database: the second projector
/// is a different process with no memory of the first.
///
/// A second event, appended after the restart, is what makes the reading
/// unambiguous. Without it a projector that silently applied nothing at all
/// would pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_a_restarted_projector_is_handed_again_is_applied_once() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    create_read_model(&pool).await;

    let system = ProcessSystem::new().await;
    let projection_id = ProjectionId::new("sqlx.exactly-once.restart");
    let stream = AggregateId::new("sqlx.exactly-once.restart.stream");
    let events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );
    let offsets = PostgresOffsetStore::new(pool.clone());

    committed(&events, stream.as_str(), vec![typed(1, INCREMENTED, b"")]).await;

    let never_fail = Arc::new(AtomicBool::new(false));
    let first_attempts = Arc::new(AtomicUsize::new(0));
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&events))
            .projection_offsets(observed(pool.clone(), &Arc::new(AtomicUsize::new(0))))
            .build(),
    );
    let first = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let attempts = Arc::clone(&first_attempts);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::clone(&attempts),
            rendezvous: None,
        }
    })
    .with_tx_provider(PoolTransactions { pool: pool.clone() })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;

    until_settled(
        "the first projector has applied the event and saved its offset",
        &pool,
        &offsets,
        &projection_id,
        1,
        Some(1),
    )
    .await;
    stopped(&system, &first).await;

    // When: a second projector takes the same projection over, and is handed
    // the event the first one already applied along with one it has not seen.
    let second_attempts = Arc::new(AtomicUsize::new(0));
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&events))
            .projection_offsets(observed(pool.clone(), &Arc::new(AtomicUsize::new(0))))
            .build(),
    );
    let second = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let attempts = Arc::clone(&second_attempts);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::clone(&attempts),
            rendezvous: None,
        }
    })
    .with_tx_provider(PoolTransactions { pool: pool.clone() })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;

    committed(&events, stream.as_str(), vec![typed(2, INCREMENTED, b"")]).await;

    until_settled(
        "the second projector has applied the new event and saved its offset",
        &pool,
        &offsets,
        &projection_id,
        2,
        Some(2),
    )
    .await;
    stopped(&system, &second).await;

    assert_eq!(
        applied(&pool).await,
        2,
        "two events were appended and the read model counts one application each; a third would \
         mean the event the first projector already applied was applied again"
    );
    assert_eq!(
        second_attempts.load(Ordering::SeqCst),
        1,
        "the second projector was handed one event it had not seen, and the offset the first \
         one left is what keeps the other from reaching it at all"
    );
}

/// Acceptance: "a rollback on a failure partway through", where `project` is
/// what fails.
///
/// The read-model update and the offset move together or not at all, so a
/// `project` that wrote the read model and then failed must leave both where
/// they were — and must leave the event unprocessed, which the run that
/// follows, after the failure is switched off, is what shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_that_fails_leaves_the_read_model_and_the_offset_where_they_were() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    create_read_model(&pool).await;

    let system = ProcessSystem::new().await;
    let projection_id = ProjectionId::new("sqlx.exactly-once.project-fails");
    let stream = AggregateId::new("sqlx.exactly-once.project-fails.stream");
    let events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );
    let offsets = PostgresOffsetStore::new(pool.clone());

    committed(&events, stream.as_str(), vec![typed(1, INCREMENTED, b"")]).await;

    let fail = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&events))
            .projection_offsets(observed(pool.clone(), &Arc::new(AtomicUsize::new(0))))
            .build(),
    );
    let projector = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&fail);
        let attempts = Arc::clone(&attempts);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::clone(&attempts),
            rendezvous: None,
        }
    })
    .with_tx_provider(PoolTransactions { pool: pool.clone() })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;

    // Two attempts rather than one: the second is only reached because the
    // event was redelivered, which is the half of the clause that says the
    // event did not become processed.
    until_counted(
        "the failing projector has been handed the same event twice",
        &[&attempts],
        2,
    )
    .await;

    assert_eq!(
        applied(&pool).await,
        0,
        "project() wrote the read model inside the framework's transaction and then failed, so \
         the rollback must have taken that write with it"
    );
    assert_eq!(
        offset(&offsets, &projection_id).await,
        None,
        "an event that was not applied leaves the projection's offset where it was"
    );

    // When: the failure is switched off, the held event is applied.
    fail.store(false, Ordering::SeqCst);

    until_settled(
        "the redelivered event has been applied and the offset saved",
        &pool,
        &offsets,
        &projection_id,
        1,
        Some(1),
    )
    .await;
    stopped(&system, &projector).await;

    assert_eq!(
        applied(&pool).await,
        1,
        "the event was applied once — the attempts that failed left nothing behind to add to"
    );
}

/// Acceptance: "a rollback on a failure partway through", where the offset
/// save is what fails.
///
/// The offset is carried past the event while the projector is already
/// running, which is what another consumer of the same projection would do.
/// The projector's own save is then behind it, the store refuses it, and the
/// read-model update it had already made inside that transaction must go with
/// the rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_offset_save_rolls_the_read_model_update_back() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    create_read_model(&pool).await;

    let system = ProcessSystem::new().await;
    let projection_id = ProjectionId::new("sqlx.exactly-once.save-refused");
    let stream = AggregateId::new("sqlx.exactly-once.save-refused.stream");
    let events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );
    let offsets = PostgresOffsetStore::new(pool.clone());

    let conflicts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let never_fail = Arc::new(AtomicBool::new(false));
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&events))
            .projection_offsets(observed(pool.clone(), &conflicts))
            .build(),
    );
    let projector = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let attempts = Arc::clone(&attempts);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::clone(&attempts),
            rendezvous: None,
        }
    })
    .with_tx_provider(PoolTransactions { pool: pool.clone() })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;

    // `spawn` has already read the offset this projector will carry, so
    // what is saved now is behind its back — the state a second consumer of the
    // same projection would leave, and the one this defence exists for.
    offsets
        .save(&OffsetScope::from(&projection_id), 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    committed(&events, stream.as_str(), vec![typed(1, INCREMENTED, b"")]).await;

    until_counted(
        "the projector's offset save has been refused as a conflict",
        &[&conflicts],
        1,
    )
    .await;
    stopped(&system, &projector).await;

    assert_eq!(
        applied(&pool).await,
        0,
        "the read-model update rode the transaction the refused offset save rolled back, so \
         nothing of it may have committed"
    );
    assert_eq!(
        offset(&offsets, &projection_id).await,
        Some(5),
        "a refused save leaves the offset where the other writer put it"
    );
}

/// Acceptance: "concurrent projectors kept from applying twice (the defence)".
///
/// Two `ProcessSystem`s, two pools, one database, one `projection_id`, and both
/// projectors holding the same offset the moment they reach the same event.
/// The rendezvous inside `project` is what puts them there together — without
/// it the first would be finished before the second began, which is the
/// sequential case this clause is not about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_projectors_holding_one_offset_apply_the_event_once() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let one_pool = schema.pool();
    let other_pool = schema.separate_pool().await;
    create_read_model(&one_pool).await;

    let projection_id = ProjectionId::new("sqlx.exactly-once.two.carried");
    let stream = AggregateId::new("sqlx.exactly-once.two.carried.stream");
    let offsets = PostgresOffsetStore::new(one_pool.clone());

    let one_events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(one_pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );
    let other_events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(other_pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );

    committed(
        &one_events,
        stream.as_str(),
        vec![typed(1, INCREMENTED, b""), typed(2, INCREMENTED, b"")],
    )
    .await;
    // The offset both projectors start from: already past the first event,
    // so the one they contest is the second.
    offsets
        .save(&OffsetScope::from(&projection_id), 1, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    let rendezvous = Arc::new(Barrier::new(2));
    let never_fail = Arc::new(AtomicBool::new(false));
    let one_conflicts = Arc::new(AtomicUsize::new(0));
    let other_conflicts = Arc::new(AtomicUsize::new(0));

    let one_system = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&one_events))
            .projection_offsets(observed(one_pool.clone(), &one_conflicts))
            .build(),
    );
    let one = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let rendezvous = Arc::clone(&rendezvous);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::new(AtomicUsize::new(0)),
            rendezvous: Some(Arc::clone(&rendezvous)),
        }
    })
    .with_tx_provider(PoolTransactions {
        pool: one_pool.clone(),
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&one_system)
    .await;

    let other_system = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&other_events))
            .projection_offsets(observed(other_pool.clone(), &other_conflicts))
            .build(),
    );
    let other = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let rendezvous = Arc::clone(&rendezvous);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::new(AtomicUsize::new(0)),
            rendezvous: Some(Arc::clone(&rendezvous)),
        }
    })
    .with_tx_provider(PoolTransactions {
        pool: other_pool.clone(),
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&other_system)
    .await;

    until_settled(
        "the contested event has been applied and the offset carried forward",
        &one_pool,
        &offsets,
        &projection_id,
        1,
        Some(2),
    )
    .await;
    until_counted(
        "the projector that lost the race has been refused as a conflict",
        &[&one_conflicts, &other_conflicts],
        1,
    )
    .await;

    stopped(&one_system, &one).await;
    stopped(&other_system, &other).await;

    assert_eq!(
        applied(&one_pool).await,
        1,
        "both projectors were handed the one contested event, and a counter that reached 2 is the \
         double application this defence exists to stop"
    );
    assert_eq!(
        offset(&offsets, &projection_id).await,
        Some(2),
        "the winner's save is the one the projection's offset carries"
    );
    let (one_conflicts, other_conflicts) = (
        one_conflicts.load(Ordering::SeqCst),
        other_conflicts.load(Ordering::SeqCst),
    );
    assert!(
        (one_conflicts == 0) != (other_conflicts == 0),
        "one projector's save is taken and the other's is refused as a conflict it can name; the \
         two reported {one_conflicts} and {other_conflicts} refusals, which is either a silent \
         double application or a projection that stopped outright"
    );
}

/// Acceptance: "two starting where no offset has been written yet".
///
/// The same defence where the projection has no offset row yet: the two
/// projectors are racing to write the first one rather than to carry an
/// existing one forward, and a rule that only guarded the second would let both
/// through here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_projectors_starting_a_projection_together_apply_the_event_once() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let one_pool = schema.pool();
    let other_pool = schema.separate_pool().await;
    create_read_model(&one_pool).await;

    let projection_id = ProjectionId::new("sqlx.exactly-once.two.first");
    let stream = AggregateId::new("sqlx.exactly-once.two.first.stream");
    let offsets = PostgresOffsetStore::new(one_pool.clone());

    let one_events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(one_pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );
    let other_events: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(other_pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );

    committed(
        &one_events,
        stream.as_str(),
        vec![typed(1, INCREMENTED, b"")],
    )
    .await;

    let rendezvous = Arc::new(Barrier::new(2));
    let never_fail = Arc::new(AtomicBool::new(false));
    let one_conflicts = Arc::new(AtomicUsize::new(0));
    let other_conflicts = Arc::new(AtomicUsize::new(0));

    let one_system = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&one_events))
            .projection_offsets(observed(one_pool.clone(), &one_conflicts))
            .build(),
    );
    let one = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let rendezvous = Arc::clone(&rendezvous);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::new(AtomicUsize::new(0)),
            rendezvous: Some(Arc::clone(&rendezvous)),
        }
    })
    .with_tx_provider(PoolTransactions {
        pool: one_pool.clone(),
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&one_system)
    .await;

    let other_system = ProcessSystem::new().await;
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(Arc::clone(&other_events))
            .projection_offsets(observed(other_pool.clone(), &other_conflicts))
            .build(),
    );
    let other = ProjectorProps::new(projection_id.clone(), &persistence, {
        let fail = Arc::clone(&never_fail);
        let rendezvous = Arc::clone(&rendezvous);
        move || CountingProjector {
            fail: Arc::clone(&fail),
            attempts: Arc::new(AtomicUsize::new(0)),
            rendezvous: Some(Arc::clone(&rendezvous)),
        }
    })
    .with_tx_provider(PoolTransactions {
        pool: other_pool.clone(),
    })
    .with_event::<Incremented>()
    .catchup_from_aggregate(stream.clone())
    .spawn(&other_system)
    .await;

    until_settled(
        "the contested event has been applied and the projection's first offset saved",
        &one_pool,
        &offsets,
        &projection_id,
        1,
        Some(1),
    )
    .await;
    until_counted(
        "the projector that lost the race has been refused as a conflict",
        &[&one_conflicts, &other_conflicts],
        1,
    )
    .await;

    stopped(&one_system, &one).await;
    stopped(&other_system, &other).await;

    assert_eq!(
        applied(&one_pool).await,
        1,
        "both projectors started a projection that had no offset at all, and a counter that \
         reached 2 is the double application this defence exists to stop"
    );
    assert_eq!(
        offset(&offsets, &projection_id).await,
        Some(1),
        "the winner's save is the projection's first offset"
    );
    let (one_conflicts, other_conflicts) = (
        one_conflicts.load(Ordering::SeqCst),
        other_conflicts.load(Ordering::SeqCst),
    );
    assert!(
        (one_conflicts == 0) != (other_conflicts == 0),
        "one projector's save is taken and the other's is refused as a conflict it can name; the \
         two reported {one_conflicts} and {other_conflicts} refusals, which is either a silent \
         double application or a projection that stopped outright"
    );
}
