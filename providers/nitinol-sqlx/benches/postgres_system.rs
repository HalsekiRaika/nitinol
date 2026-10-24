//! What it costs to bring an aggregate back out of Postgres, and for an
//! `ExactlyOnce` projection to catch up with a stream held there.
//!
//! The numbers here are a baseline rather than a target, and they belong to the
//! database they were taken against, the same as the store benchmarks in
//! `postgres.rs`. Where the database comes from and the schema a run writes in
//! are stated once, in `common/postgres.rs`.
//!
//! # The two ways an aggregate comes back, against a database
//!
//! `postgres_activation/replay/N` reads a whole stream of N rows back, and
//! `postgres_activation/snapshot/N` reads a snapshot taken at N and replays
//! nothing after it. They are the in-memory `activation` benchmarks of
//! `nitinol-eventsource` carried over to this backend, window for window, so
//! that what a snapshot saves is read where the rows have to be fetched and
//! decoded rather than handed over from memory.
//!
//! # What an `ExactlyOnce` projection pays per event
//!
//! `postgres_projection/exactly_once/N` starts a projector carrying a
//! `TxProvider` over the run's pool and waits until it has applied all N
//! records of a stream written before it existed. For every record the
//! framework begins a transaction, hands it to `project`, saves the offset in
//! it and commits, so the number is that boundary N times over — the commit
//! above all, which is what a granularity other than one transaction per event
//! would change.
//!
//! The read model is a counter in memory rather than a table. A table would put
//! one more statement inside each of those transactions and not one more
//! commit, and what is being measured is the boundary the framework draws, not
//! the statement a particular read model issues inside it.
//!
//! The projector names no poll interval, so it polls at the framework's
//! default. That cadence is not in the window: a poller reads as soon as it
//! starts, and that read asks for the whole stream with no limit, so a
//! projector that applies every record without failing never waits for a
//! second turn.
//!
//! The window ends when `project` has run for the last record, which is before
//! that record's offset save and commit: the window holds N − 1 commits, not N.
//! That is about 1 % under the boundary's cost at N = 100 and about 0.1 % at
//! N = 1000.
//!
//! # What stays behind between iterations
//!
//! A live aggregate cannot be stopped from outside it, so every activation
//! iteration leaves one idle activation on the process system the group
//! shares; the group's `sample_size` and `measurement_time` are what bound how
//! many. The activations only read, so the rows they read are the seeded ones
//! and no more.
//!
//! A projector can be stopped, and each iteration stops the one it started and
//! waits until it is gone before the next one begins, so no iteration shares
//! the pool with the tail of the one before it. Each iteration's projection
//! has an identifier of its own and leaves its offset row behind: one row per
//! iteration in a table no projector reads twice.
//!
//! # How long a run takes
//!
//! Three to five minutes on the machine `BENCHMARKS.md` records, almost all of
//! it `exactly_once/1000`: a commit is about 14 ms there, so one iteration of it
//! is about 14 s, and criterion's flat sampling runs one warm-up iteration and
//! ten samples of at least one iteration each whatever the measurement time.
//! The upper end is a run whose commits slow down for a stretch, which that
//! machine has shown. The activations together take seconds.
//!
//! Run with
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo bench -p nitinol-sqlx --bench postgres_system`.

#[path = "common/postgres.rs"]
mod postgres;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput};
use jiff::Timestamp;
use linkme::distributed_slice;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, Event, Latest, Never, ProjectionContext, ProjectionId, Projector, Query,
    Snapshotable, TxProvider,
};
use nitinol_persistence::provider::{PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventRegistryEntry, EventType, Family, PersistedSnapshot,
    TypeName, EVENT_REGISTRY,
};
use nitinol_runtime::process::{Process, ProcessProxy};
use nitinol_runtime::ProcessSystem;
use nitinol_sqlx::PostgresStore;
use postgres::BenchDatabase;
use sqlx::{PgPool, PgTransaction};
use tokio::runtime::Runtime;
use tokio::sync::Notify;

const ACTIVATED: [u64; 4] = [1, 100, 1_000, 10_000];

/// Each stream length with the measurement time its projection benchmark is
/// given: room for about two catch-ups at a commit of about 14 ms per record.
/// Criterion's flat sampling takes ten samples of at least one iteration
/// regardless, so this sizes the samples and does not shorten the run.
const PROJECTED: [(u64, Duration); 2] = [
    (100, Duration::from_secs(3)),
    (1_000, Duration::from_secs(30)),
];

/// Short, for the reason the crate-level note gives: the activation benchmarks
/// leave an idle activation behind per iteration, and warm-up iterations are
/// iterations.
const WARM_UP: Duration = Duration::from_millis(300);

const ACTIVATION_MEASUREMENT: Duration = Duration::from_secs(1);

/// Far past the ≈ 14 s a catch-up of the longest stream takes, so that only a
/// catch-up that is not going to finish trips it rather than a slow one.
const CATCHUP_DEADLINE: Duration = Duration::from_secs(120);

/// How long a stopped projector has to leave the registry, and how often that
/// is looked at.
const STOP_DEADLINE: Duration = Duration::from_secs(30);
const STOP_POLL: Duration = Duration::from_millis(25);

/// Fixed rather than read off the clock, so two runs seed the same rows.
const SEEDED_AT_MICROSECOND: i64 = 1_700_000_000_123_456;

/// Each iteration needs an identifier of its own: a projection's applied
/// position is kept under it, and an identifier used twice would have the
/// second iteration start where the first finished and count nothing.
static PROJECTIONS: AtomicU64 = AtomicU64::new(0);

const INCREMENTED: EventType = EventType::new(
    Family::new("bench.sqlx.system"),
    TypeName::new("Incremented"),
);

/// What makes `INCREMENTED` restorable: a store serves the event types its
/// binary linked, so one a benchmark appends is declared to the collection the
/// same way a domain event is.
#[distributed_slice(EVENT_REGISTRY)]
static INCREMENTED_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::INCREMENTED"), &[INCREMENTED]);

#[derive(Clone)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = INCREMENTED;

    type Codec = CountCodec;
}

/// The one event this file writes carries nothing, so its codec writes nothing.
///
/// A payload would put serialization in numbers that are about activation and
/// the transaction boundary.
struct CountCodec;

impl Codec<Incremented> for CountCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("a snapshot of a counter is 8 bytes, got {0}")]
struct CountDecodeError(usize);

impl Codec<u64> for CountCodec {
    type Error = CountDecodeError;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        payload
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| CountDecodeError(payload.len()))
    }
}

/// The aggregate that comes back by replaying everything.
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

/// The aggregate that comes back from the newest snapshot, then replays what
/// followed it.
#[derive(Default)]
struct SnapshotCounter {
    value: u64,
}

impl Aggregate for SnapshotCounter {
    type Event = Incremented;
    type Recover = Latest;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

impl Snapshotable for SnapshotCounter {
    type Snapshot = u64;

    type Codec = CountCodec;

    fn capture(&self) -> u64 {
        self.value
    }

    fn recover(snapshot: u64) -> Self {
        Self { value: snapshot }
    }
}

/// How far the activation got, which is what says it recovered at all.
struct GetCount;

impl Query<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

impl Query<GetCount> for SnapshotCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: GetCount) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}

/// A projection whose read model is how many records it has applied.
///
/// It is handed the framework's transaction and writes nothing in it, for the
/// reason the crate-level note gives.
struct CountingProjector {
    applied: Arc<AtomicU64>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Projector<Incremented, PgTransaction<'static>> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, PgTransaction<'static>>,
    ) -> Result<(), Self::Error> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        self.reached.notify_one();
        Ok(())
    }
}

/// The `TxProvider` a user writes to put this provider's offset store under an
/// `ExactlyOnce` projector.
///
/// `nitinol-sqlx` defines none: a transaction over a `PgPool` is that pool's
/// own `begin`, `commit` and `rollback`, and a provider that wrapped them would
/// have to depend on the interpreter crate that declares `TxProvider` — the
/// wrong way round for a provider. So the benchmark carries the one a user
/// would write, which is what makes the number the one a deployment pays.
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

fn store_over(pool: &PgPool) -> PostgresStore {
    PostgresStore::new(pool.clone())
        .expect("the event types this benchmark binary linked claim distinct canonical paths")
}

/// A system over `pool` with this backend's whole store behind it, so `Latest`
/// recovery and the snapshot persistor reach the snapshot table.
fn system_over(ps: &ProcessSystem, pool: &PgPool) -> EventSourceSystem<PostgresStore> {
    EventSourceSystem::builder(ps.clone())
        .with_persistence(PersistenceProvider::new(store_over(pool)))
        .build()
}

fn fixed_instant() -> Timestamp {
    Timestamp::from_microsecond(SEEDED_AT_MICROSECOND)
        .expect("a microsecond count inside the supported range is a timestamp")
}

/// `count` records numbered from 1, which is what a stream holding nothing
/// accepts.
fn batch(count: u64) -> Vec<AppendingEvent> {
    (1..=count)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type: INCREMENTED,
            payload: Bytes::new(),
            occurred_at: fixed_instant(),
        })
        .collect()
}

/// Written through the journal in one `append` rather than through an
/// activation: what the benchmarks measure is reading a stream back, and an
/// activation writing it would be a second thing in the setup with its own
/// cost.
///
/// Every stream this run seeds has a key of its own — the run's schema is one
/// for all of them, and a second seed under one key would be refused from
/// sequence 1.
async fn seeded(store: &PostgresStore, id: &AggregateId, count: u64) {
    store
        .events()
        .append(id.as_str(), batch(count))
        .await
        .expect("seeding one stream from sequence 1 is an append nothing has taken");
}

/// The same, plus a snapshot taken at `count`.
///
/// The snapshot's sequence is the last record's, so an activation recovering
/// from it replays nothing — which is the thing being measured against a
/// replay of the whole stream.
async fn seeded_with_snapshot(store: &PostgresStore, id: &AggregateId, count: u64) {
    seeded(store, id, count).await;
    store
        .snapshots()
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: count,
            payload: CountCodec::encode(&count).expect("a counter's value is eight bytes"),
            created_at: fixed_instant(),
        })
        .await
        .expect("saving the one snapshot of a seeded stream must be taken");
}

/// A projection identifier no iteration in this process has used yet.
fn fresh_projection() -> ProjectionId {
    ProjectionId::new(format!(
        "bench-projection-{}",
        PROJECTIONS.fetch_add(1, Ordering::Relaxed)
    ))
}

async fn applied_to(applied: &AtomicU64, reached: &Notify, expected: u64) {
    let watch = async {
        loop {
            let notified = reached.notified();
            if applied.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    };
    tokio::time::timeout(CATCHUP_DEADLINE, watch)
        .await
        .unwrap_or_else(|_| {
            panic!("a projector following a stream of {expected} records must apply all of them")
        });
}

/// Stop `proxy` and wait for the runtime to let go of it.
///
/// The window closed when `project` ran for the last record, while that
/// record's offset save and commit were still on their way. Waiting for the
/// process to leave the registry, rather than for the stop to have been sent,
/// is what keeps that tail out of the next iteration's window.
async fn stopped<P: Process>(system: &ProcessSystem, proxy: &ProcessProxy<P>) {
    proxy
        .stop()
        .await
        .expect("a projector this iteration started must take a stop");
    let pid = proxy.pid();
    let gone = async {
        while system.lookup(pid).await.is_some() {
            tokio::time::sleep(STOP_POLL).await;
        }
    };
    tokio::time::timeout(STOP_DEADLINE, gone)
        .await
        .expect("a projector that was told to stop must leave the runtime's registry");
}

/// What bringing one aggregate back out of Postgres costs, by replaying its
/// whole stream and by reading a snapshot that covers it.
///
/// Timed: resolving the identity on a system that has no activation for it —
/// which starts one — and the `exec` that reads how far it got. Those two are
/// one window because the activation recovers while the query waits behind it:
/// the query returning the stream's length is what says the recovery finished.
///
/// Not timed: seeding, building each iteration's system, and starting the
/// snapshot persistor the `snapshot` benchmark's aggregate recovers through.
/// A persistor is one process per system and this benchmark builds a system per
/// iteration, so leaving it in the window would measure a process start that a
/// deployment pays once.
///
/// Each iteration builds a system of its own over the run's one pool. That is
/// what makes every iteration a recovery: a system resolves one activation per
/// identity, so asking the same system twice would reach the state already in
/// memory and measure nothing being read back at all.
fn activation(c: &mut Criterion, rt: &Runtime, pool: &PgPool) {
    let ps = rt.block_on(ProcessSystem::new());
    let seeder = store_over(pool);

    let mut group = c.benchmark_group("postgres_activation");
    group.sample_size(10);
    group.warm_up_time(WARM_UP);
    group.measurement_time(ACTIVATION_MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    for count in ACTIVATED {
        let replayed = AggregateId::new(format!("bench-activation-replayed-{count}"));
        let snapshotted = AggregateId::new(format!("bench-activation-snapshotted-{count}"));
        rt.block_on(seeded(&seeder, &replayed, count));
        rt.block_on(seeded_with_snapshot(&seeder, &snapshotted, count));

        group.throughput(Throughput::Elements(count));

        group.bench_with_input(BenchmarkId::new("replay", count), &count, |b, &count| {
            b.to_async(rt).iter_custom(|iters| {
                let ps = ps.clone();
                let pool = pool.clone();
                let id = replayed.clone();
                async move {
                    let mut measured = Duration::ZERO;
                    for _ in 0..iters {
                        let system = system_over(&ps, &pool);

                        let started = Instant::now();
                        let aggregate = system.spawn_aggregate::<Counter>(id.clone()).await;
                        let value = aggregate
                            .exec(GetCount)
                            .await
                            .expect("querying a recovered counter must be answered");
                        measured += started.elapsed();

                        assert_eq!(
                            value, count,
                            "a replay of the whole stream must leave the counter at its length"
                        );
                    }
                    measured
                }
            });
        });

        group.bench_with_input(BenchmarkId::new("snapshot", count), &count, |b, &count| {
            b.to_async(rt).iter_custom(|iters| {
                let ps = ps.clone();
                let pool = pool.clone();
                let id = snapshotted.clone();
                async move {
                    let mut measured = Duration::ZERO;
                    for _ in 0..iters {
                        let system = system_over(&ps, &pool);
                        system.snapshot_persistor().await;

                        let started = Instant::now();
                        let aggregate = system.spawn_aggregate::<SnapshotCounter>(id.clone()).await;
                        let value = aggregate
                            .exec(GetCount)
                            .await
                            .expect("querying a recovered counter must be answered");
                        measured += started.elapsed();

                        assert_eq!(
                            value, count,
                            "a snapshot taken at the stream's length must leave the counter there \
                             with nothing replayed after it"
                        );
                    }
                    measured
                }
            });
        });
    }

    group.finish();
}

/// What it costs a projection running `ExactlyOnce` to catch up with a stream
/// that was written before it existed.
///
/// Timed: starting the projector and waiting until its read model has counted
/// every record in the stream.
///
/// Not timed: seeding, the system, and stopping the projector afterwards.
///
/// The system is one per stream length: a projector is resolved by its
/// projection identifier, which is new every iteration, so a system asked
/// again starts a projector from nothing all the same.
fn exactly_once(c: &mut Criterion, rt: &Runtime, pool: &PgPool) {
    let ps = rt.block_on(ProcessSystem::new());
    let seeder = store_over(pool);

    let mut group = c.benchmark_group("postgres_projection");
    group.sample_size(10);
    group.warm_up_time(WARM_UP);
    group.sampling_mode(SamplingMode::Flat);

    for (count, measurement) in PROJECTED {
        let followed = AggregateId::new(format!("bench-projection-followed-{count}"));
        rt.block_on(seeded(&seeder, &followed, count));
        let system = system_over(&ps, pool);
        let system = &system;

        group.measurement_time(measurement);
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("exactly_once", count),
            &count,
            |b, &count| {
                b.to_async(rt).iter_custom(|iters| {
                    let pool = pool.clone();
                    let id = followed.clone();
                    async move {
                        let mut measured = Duration::ZERO;
                        for _ in 0..iters {
                            let applied = Arc::new(AtomicU64::new(0));
                            let reached = Arc::new(Notify::new());
                            let projector_applied = Arc::clone(&applied);
                            let projector_reached = Arc::clone(&reached);
                            let projection = fresh_projection();

                            let started = Instant::now();
                            let projector = system
                                .projector_props(projection, move || CountingProjector {
                                    applied: Arc::clone(&projector_applied),
                                    reached: Arc::clone(&projector_reached),
                                })
                                .with_tx_provider(PoolTransactions { pool: pool.clone() })
                                .with_event::<Incremented>()
                                .catchup_from_aggregate(id.clone())
                                .spawn()
                                .await;
                            applied_to(&applied, &reached, count).await;
                            measured += started.elapsed();

                            stopped(system.process_system(), &projector).await;
                        }
                        measured
                    }
                });
            },
        );
    }

    group.finish();
}

/// Written out rather than left to `criterion_main!` because the database this
/// run writes in is brought up before any benchmark and taken away after all of
/// them, and because the absence of one ends the run before a benchmark is
/// registered at all.
///
/// What it does otherwise is what `criterion_main!` does: configure from the
/// command line, so `--save-baseline`, `--baseline` and `--profile-time` reach
/// the benchmarks below, and write the summary at the end.
fn main() {
    let Some(url) = postgres::database_url() else {
        return;
    };

    let rt = postgres::runtime();
    let db = rt.block_on(BenchDatabase::bring_up(&url));

    let mut criterion = Criterion::default().configure_from_args();
    activation(&mut criterion, &rt, &db.pool);
    exactly_once(&mut criterion, &rt, &db.pool);
    criterion.final_summary();

    rt.block_on(db.tear_down());
}
