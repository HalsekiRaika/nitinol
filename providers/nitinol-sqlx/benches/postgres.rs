//! What this backend's two write paths cost against a real Postgres: appending
//! to a stream, and saving an offset inside the caller's transaction.
//!
//! The numbers here are a baseline rather than a target, and they belong to the
//! database they were taken against: a managed instance across a network and a
//! container on the same machine are two different measurements, so whatever is
//! recorded carries what it was run on.
//!
//! # It runs against `DATABASE_URL` or it does not run
//!
//! There is no fallback that starts a database. A throwaway container brought
//! up per run measures that container's cold caches and its host's spare
//! capacity, which is not a baseline anything can be compared against later.
//! So with no `DATABASE_URL` this says so on stderr and exits successfully,
//! having registered no benchmark at all.
//!
//! # The schema it writes in
//!
//! A schema of this run's own, named after the process, migrated on the way in
//! and dropped on the way out. A panic leaves it behind — the drop is on the
//! way out of a successful run and nowhere else — so a crashed run is cleaned
//! up by hand.
//!
//! # What the rows do to the numbers
//!
//! Both benchmarks write, and nothing deletes: a later iteration appends into a
//! fuller table than an earlier one, so the median is over a run that got
//! steadily slower as its indexes grew. How far that goes is bounded by each
//! group's `sample_size` and `measurement_time` and by nothing else.
//!
//! Run with
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo bench -p nitinol-sqlx`.

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use criterion::{BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput};
use jiff::Timestamp;
use linkme::distributed_slice;
use nitinol_persistence::store::{EventStore, OffsetScope, OffsetStore};
use nitinol_persistence::{
    AppendingEvent, EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY,
};
use nitinol_sqlx::{PostgresEventStore, PostgresOffsetStore};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Executor as _, PgPool};
use tokio::runtime::Runtime;

const RECORDED: EventType = EventType::new(Family::new("bench.sqlx"), TypeName::new("Recorded"));

/// What makes `RECORDED` restorable: a store serves the event types its binary
/// linked, so one a benchmark appends is declared to the collection the same
/// way a domain event is.
#[distributed_slice(EVENT_REGISTRY)]
static RECORDED_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::RECORDED"), &[RECORDED]);

const PAYLOAD: &[u8] = b"bench";

/// Fixed rather than read off the clock, so two runs append the same rows.
const OCCURRED_AT_MICROSECOND: i64 = 1_700_000_000_123_456;

const APPENDED: [u64; 2] = [1, 100];

/// Each benchmark has one call in flight at a time, so a handful is enough for
/// the pool never to be what a measurement is waiting on.
const POOL_CONNECTIONS: u32 = 4;

const WARM_UP: Duration = Duration::from_secs(1);

/// Short for the reason the crate-level note gives: every iteration writes a
/// row nothing removes.
const MEASUREMENT: Duration = Duration::from_secs(2);

static STREAMS: AtomicU64 = AtomicU64::new(0);

/// An offset moves forward or not at all, so a save at a sequence already
/// reached is refused — which is why this counts rather than repeating a value.
static SAVED: AtomicU64 = AtomicU64::new(0);

/// The Postgres this run writes in, and the two stores over it.
struct Benched {
    schema: String,
    /// The connection the schema is created and dropped through, which is not
    /// the one the benchmarks run on: that one's `search_path` is already
    /// pointed at the schema.
    admin: PgPool,
    pool: PgPool,
    events: PostgresEventStore,
    offsets: PostgresOffsetStore,
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a benchmark run needs a tokio runtime to drive the database on")
}

fn fixed_instant() -> Timestamp {
    Timestamp::from_microsecond(OCCURRED_AT_MICROSECOND)
        .expect("a microsecond count inside the supported range is a timestamp")
}

/// A stream key no append in this process has used yet.
fn fresh_stream() -> String {
    format!("bench-append-{}", STREAMS.fetch_add(1, Ordering::Relaxed))
}

/// `count` records numbered from 1, which is what a stream holding nothing
/// accepts.
fn batch(count: u64) -> Vec<AppendingEvent> {
    (1..=count)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type: RECORDED,
            payload: Bytes::from_static(PAYLOAD),
            occurred_at: fixed_instant(),
        })
        .collect()
}

async fn pool_on(url: &str, schema: &str) -> PgPool {
    let schema = schema.to_owned();
    PgPoolOptions::new()
        .max_connections(POOL_CONNECTIONS)
        .after_connect(move |connection, _| {
            let search_path = format!("SET search_path TO {schema}");
            Box::pin(async move {
                connection.execute(AssertSqlSafe(search_path)).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("the database DATABASE_URL names must accept a pool of connections")
}

/// A schema of this run's own, migrated, with a store of each kind over it.
async fn bring_up(url: &str) -> Benched {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("the database DATABASE_URL names must accept a connection");

    // A schema name cannot be a bind parameter. This one is built here out of a
    // process id, so the statement carries nothing a caller could have shaped.
    let schema = format!("nitinol_sqlx_bench_{}", process::id());
    admin
        .execute(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .await
        .expect("the database DATABASE_URL names must allow a schema of its own");

    let pool = pool_on(url, &schema).await;
    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider's migrations must apply to the database this run reached");

    let events = PostgresEventStore::new(pool.clone())
        .expect("the event types this benchmark binary linked claim distinct canonical paths");
    let offsets = PostgresOffsetStore::new(pool.clone());

    Benched {
        schema,
        admin,
        pool,
        events,
        offsets,
    }
}

/// Give the connections back and take the schema away again.
async fn tear_down(benched: Benched) {
    // The two stores hold handles onto `pool` and are let go of here, before
    // the connections they reach through are given back.
    let Benched {
        schema,
        admin,
        pool,
        ..
    } = benched;
    pool.close().await;

    admin
        .execute(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .await
        .expect("the schema this run created must be removable again");
    admin.close().await;
}

/// What one `append` costs, as a single record and as a batch of a hundred.
///
/// Timed: the `append` call, which is one round trip to the database.
///
/// Not timed: the stream key and the records each iteration appends, both built
/// in the setup `iter_batched` keeps outside the window.
///
/// Every iteration appends to a stream of its own, numbered from 1, so no
/// iteration is refused for a sequence another iteration has taken.
fn event_store(c: &mut Criterion, rt: &Runtime, benched: &Benched) {
    let store = &benched.events;
    let mut group = c.benchmark_group("postgres_event_store");
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    for count in APPENDED {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("append", count), &count, |b, &count| {
            b.to_async(rt).iter_batched(
                || (fresh_stream(), batch(count)),
                |(key, events)| async move {
                    store
                        .append(&key, events)
                        .await
                        .expect("an append to a stream of its own from sequence 1 must be taken")
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// What it costs a projection running `ExactlyOnce` to record where it got to.
///
/// Timed: beginning the transaction, the save inside it, and the commit. All
/// three, because what a projection pays is the boundary and not the statement:
/// the offset landing with the read-model update it covers is the whole point
/// of handing the store a transaction.
///
/// Not timed: nothing. The sequence each iteration saves comes off a counter,
/// which is inside the window and costs an atomic add.
fn offset_store(c: &mut Criterion, rt: &Runtime, benched: &Benched) {
    let store = &benched.offsets;
    let pool = &benched.pool;
    let scope = OffsetScope::new("bench.sqlx.offset");
    let scope = &scope;

    let mut group = c.benchmark_group("postgres_offset_store");
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    group.bench_function("save_in_tx", |b| {
        b.to_async(rt).iter(|| async move {
            let sequence = SAVED.fetch_add(1, Ordering::Relaxed) + 1;
            let mut tx = pool
                .begin()
                .await
                .expect("the database this run reached must begin a transaction");
            store
                .save(scope, sequence, Some(&mut tx))
                .await
                .expect("a save that carries the offset forward must be taken");
            tx.commit()
                .await
                .expect("a transaction the store wrote in must commit");
        });
    });

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
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "nitinol-sqlx benches: nothing to run against, so nothing was measured \
             (no DATABASE_URL)"
        );
        return;
    };

    let rt = runtime();
    let benched = rt.block_on(bring_up(&url));

    let mut criterion = Criterion::default().configure_from_args();
    event_store(&mut criterion, &rt, &benched);
    offset_store(&mut criterion, &rt, &benched);
    criterion.final_summary();

    rt.block_on(tear_down(benched));
}
