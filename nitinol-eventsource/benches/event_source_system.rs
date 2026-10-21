//! What it costs to bring an aggregate back, and for a projection to catch up,
//! over the in-memory store.
//!
//! The numbers here are a baseline rather than a target. Nothing in this file
//! is a claim about what any of it ought to cost — it is what it costs today,
//! so that a later change can be told apart from noise.
//!
//! # The two ways an aggregate comes back, side by side
//!
//! `activation/replay/N` reads a whole stream of N records, and
//! `activation/snapshot/N` reads a snapshot taken at N and replays nothing.
//! They are one group so that the two are read against each other at the same
//! stream length, which is the comparison a deployment deciding whether to
//! snapshot is making.
//!
//! # What stays behind between iterations
//!
//! A live aggregate cannot be stopped from outside it, so every iteration of
//! the activation benchmarks leaves one idle activation on the process system
//! they share. How many that is, is bounded by the group's `sample_size` and
//! `measurement_time` and by nothing else, which is why both are set low here.
//!
//! A projector *can* be stopped, and each iteration stops the one it started:
//! a projector left running would keep asking the store for what is new every
//! 250 ms, and the iterations after it would be measured against that load.
//!
//! The seeded store is shared across the iterations of one benchmark and never
//! written to by them, so the cost of reading it does not drift as they run.
//!
//! Run with `cargo bench -p nitinol-eventsource`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use jiff::Timestamp;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, Event, Latest, Never, ProjectionContext, ProjectionId, Projector, Query,
    Snapshotable,
};
use nitinol_persistence::provider::{PersistenceProvider, SnapshotSlot, Store};
use nitinol_persistence::store::InMemoryStore;
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, PersistedSnapshot, TypeName,
};
use nitinol_runtime::ProcessSystem;
use tokio::runtime::Runtime;
use tokio::sync::Notify;

const SEEDED: [u64; 3] = [1, 100, 1_000];

/// Short, for the reason the crate-level note gives: the activation benchmarks
/// leave an idle activation behind per iteration, and warm-up iterations are
/// iterations.
const WARM_UP: Duration = Duration::from_millis(300);

const MEASUREMENT: Duration = Duration::from_secs(1);

/// Longer than the activations' because a catch-up is three orders of
/// magnitude longer: a window that fits one catch-up per sample leaves
/// criterion with a sample of a single iteration and a warning saying so, and
/// this is the shortest window that fits two.
const CATCHUP_MEASUREMENT: Duration = Duration::from_secs(5);

/// A catch-up that never completes would otherwise hang the whole run with
/// nothing said about why.
const CATCHUP_DEADLINE: Duration = Duration::from_secs(30);

/// Each iteration needs an identifier of its own: a projection's applied
/// position is kept under it, and an identifier used twice would have the
/// second iteration start where the first finished and count nothing.
static PROJECTIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("bench.eventsource"),
        TypeName::new("Incremented"),
    );

    type Codec = CountCodec;
}

/// The one event this file writes carries nothing, so its codec writes nothing.
///
/// A payload would put serialization in numbers that are about activation and
/// catch-up.
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
struct CountingProjector {
    applied: Arc<AtomicU64>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Projector<Incremented> for CountingProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Incremented,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        self.reached.notify_one();
        Ok(())
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a benchmark run needs a tokio runtime to drive the system on")
}

/// `count` records numbered from 1, which is what a stream holding nothing
/// accepts.
fn batch(count: u64) -> Vec<AppendingEvent> {
    (1..=count)
        .map(|sequence| AppendingEvent {
            sequence,
            event_type: <Incremented as Event>::EVENT_TYPE,
            payload: Bytes::new(),
            occurred_at: Timestamp::now(),
        })
        .collect()
}

/// Written through the journal rather than through an activation: what the
/// benchmarks measure is reading a stream back, and an activation writing it
/// would be a second thing in the setup with its own cost.
async fn seeded(id: &AggregateId, count: u64) -> InMemoryStore {
    let store = InMemoryStore::default();
    store
        .events()
        .append(id.as_str(), batch(count))
        .await
        .expect("seeding one stream from sequence 1 is an append nothing has taken");
    store
}

/// The same, plus a snapshot taken at `count`.
///
/// The snapshot's sequence is the last record's, so an activation recovering
/// from it replays nothing — which is the thing being measured against a
/// replay of the whole stream.
async fn seeded_with_snapshot(id: &AggregateId, count: u64) -> InMemoryStore {
    let store = seeded(id, count).await;
    store
        .snapshots()
        .save(PersistedSnapshot {
            aggregate_id: id.clone(),
            sequence: count,
            payload: CountCodec::encode(&count).expect("a counter's value is eight bytes"),
            created_at: Timestamp::now(),
        })
        .await
        .expect("saving the one snapshot of a seeded stream must be taken");
    store
}

/// A projection identifier no iteration in this process has used yet.
fn fresh_projection() -> ProjectionId {
    ProjectionId::new(format!(
        "bench-catchup-{}",
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

/// What bringing one aggregate back costs, by replaying its whole stream and by
/// reading a snapshot that covers it.
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
/// Each iteration builds a system of its own over the one seeded store. That is
/// what makes every iteration a recovery: a system resolves one activation per
/// identity, so asking the same system twice would reach the state already in
/// memory and measure nothing being read back at all.
fn activation(c: &mut Criterion) {
    let rt = runtime();
    let ps = rt.block_on(ProcessSystem::new());
    let replayed = AggregateId::new("bench-activation-replayed");
    let snapshotted = AggregateId::new("bench-activation-snapshotted");

    let mut group = c.benchmark_group("activation");
    group.sample_size(10);
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    for count in SEEDED {
        let replay_store = rt.block_on(seeded(&replayed, count));
        let snapshot_store = rt.block_on(seeded_with_snapshot(&snapshotted, count));

        group.throughput(Throughput::Elements(count));

        group.bench_with_input(BenchmarkId::new("replay", count), &count, |b, &count| {
            b.to_async(&rt).iter_custom(|iters| {
                let ps = ps.clone();
                let store = replay_store.clone();
                let id = replayed.clone();
                async move {
                    let mut measured = Duration::ZERO;
                    for _ in 0..iters {
                        let system = EventSourceSystem::builder(ps.clone())
                            .with_persistence(PersistenceProvider::new(store.clone()))
                            .build();

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
            b.to_async(&rt).iter_custom(|iters| {
                let ps = ps.clone();
                let store = snapshot_store.clone();
                let id = snapshotted.clone();
                async move {
                    let mut measured = Duration::ZERO;
                    for _ in 0..iters {
                        let system = EventSourceSystem::builder(ps.clone())
                            .with_persistence(PersistenceProvider::new(store.clone()))
                            .build();
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

/// What it costs a projection to catch up with a stream that was written before
/// it existed.
///
/// Timed: starting the projector and waiting until its read model has counted
/// every record in the stream.
///
/// Not timed: seeding, the system, and stopping the projector afterwards.
///
/// # The default poll interval is inside the window
///
/// A projector starts a poller, and that poller waits out its interval before
/// its first read rather than reading straight away. The interval a projector
/// polls at is the framework's default of 250 ms and there is no entry point
/// that changes it, so **each iteration carries about 250 ms that is the
/// cadence and not the catch-up**. What the numbers here compare across stream
/// lengths is therefore the work above that floor, and the floor is why the
/// three lengths read so close to one another.
fn projector_catchup(c: &mut Criterion) {
    let rt = runtime();
    let ps = rt.block_on(ProcessSystem::new());
    let followed = AggregateId::new("bench-catchup-followed");

    let mut group = c.benchmark_group("projector_catchup");
    group.sample_size(10);
    group.warm_up_time(WARM_UP);
    group.measurement_time(CATCHUP_MEASUREMENT);
    group.sampling_mode(SamplingMode::Flat);

    for count in SEEDED {
        let system = EventSourceSystem::builder(ps.clone())
            .with_persistence(PersistenceProvider::new(
                rt.block_on(seeded(&followed, count)),
            ))
            .build();
        let system = &system;

        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("from_aggregate", count),
            &count,
            |b, &count| {
                b.to_async(&rt).iter_custom(|iters| {
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
                                .with_event::<Incremented>()
                                .catchup_from_aggregate(id.clone())
                                .spawn()
                                .await;
                            applied_to(&applied, &reached, count).await;
                            measured += started.elapsed();

                            projector
                                .stop()
                                .await
                                .expect("a projector this iteration started must take a stop");
                        }
                        measured
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(event_source_system, activation, projector_catchup);
criterion_main!(event_source_system);
