//! What the in-memory journal costs on the two calls every deployment makes:
//! appending to a stream, and reading one back.
//!
//! The numbers here are a baseline rather than a target. Nothing in this file
//! is a claim about what the store ought to cost — it is what the store costs
//! today, so that a later change to it can be told apart from noise.
//!
//! # What is timed, and what is not
//!
//! Each benchmark below states its own timed window. Two things are outside
//! every one of them: building the records an append is handed, and seeding the
//! store a load reads from.
//!
//! # What this store's shape does to the numbers
//!
//! A load walks every record the store holds and copies it before it filters,
//! so its cost is in the size of the whole store rather than of the stream
//! asked for. That is why each stream length gets a store of its own here,
//! written once and never written to again: the three lengths are comparable
//! because each store holds only the length it is measuring.
//!
//! An append reads the sequences of the one stream it is appending to and files
//! that stream in a table of streams, so what its cost grows with is how many
//! streams the store holds. Each iteration therefore gets a store of its own as
//! well, for the reason stated on [`append`].
//!
//! Run with `cargo bench -p nitinol-persistence`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use futures_util::StreamExt;
use jiff::Timestamp;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, TypeName};
use tokio::runtime::Runtime;

const RECORDED: EventType =
    EventType::new(Family::new("bench.persistence"), TypeName::new("Recorded"));

/// Fixed and small: what is under measurement is the store's bookkeeping, and a
/// payload the store only moves around would put the allocator in the numbers
/// instead.
const PAYLOAD: &[u8] = b"bench";

const APPENDED: [u64; 2] = [1, 100];

const LOADED: [u64; 3] = [1, 100, 1_000];

const SEEDED_STREAM: &str = "bench-load";

const WARM_UP: Duration = Duration::from_secs(1);

/// Below criterion's default. Both calls are deterministic and neither drifts
/// across a run, so a shorter window lands on the same numbers, and it is what
/// keeps a whole run of this crate's benchmarks inside a minute.
const MEASUREMENT: Duration = Duration::from_secs(2);

static STREAMS: AtomicU64 = AtomicU64::new(0);

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
            occurred_at: Timestamp::now(),
        })
        .collect()
}

async fn seeded(count: u64) -> InMemoryEventStore {
    let store = InMemoryEventStore::default();
    store
        .append(SEEDED_STREAM, batch(count))
        .await
        .expect("seeding one stream from sequence 1 is an append nothing has taken");
    store
}

/// Read everything `query` selects, and answer how much that was.
///
/// The count is answered rather than dropped so that the benchmark is charged
/// for pulling the whole stream: a routine that dropped the stream unread would
/// be timing the load call and nothing after it.
async fn drain(store: &InMemoryEventStore, query: LoadQuery) -> usize {
    let mut stream = store
        .load(query)
        .await
        .expect("a load against a store holding what this benchmark seeded must be served");
    let mut read = 0usize;
    while let Some(record) = stream.next().await {
        record.expect("every record served back is one the store took");
        read += 1;
    }
    read
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a benchmark run needs a tokio runtime to drive the store on")
}

/// What one `append` costs, as a single record and as a batch of a hundred.
///
/// Timed: the `append` call, and nothing else.
///
/// Not timed: the store, the stream key and the records each iteration appends,
/// all three built in the setup `iter_batched` keeps outside the window — and
/// the store's disposal, which lands outside it too because the routine hands
/// the store back as its result.
///
/// Every iteration appends under a key of its own, from sequence 1, so no
/// iteration is refused for a sequence another iteration has taken.
///
/// # A store of its own per iteration
///
/// What an append's cost grows with is the number of streams the store holds,
/// and a key of its own per iteration is exactly what grows that. Sharing one
/// store across the group would therefore have the numbers measure a table of
/// streams being grown to however many iterations criterion chose for the run
/// — millions of them, drifting upward throughout, and a different count every
/// time the window or the machine changes.
///
/// So each iteration appends into a store holding nothing. What that reports is
/// the cost of the call, and it reports the same thing on the next run.
fn append(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("in_memory_append");
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);

    for count in APPENDED {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("events", count), &count, |b, &count| {
            b.to_async(&rt).iter_batched(
                || (InMemoryEventStore::default(), fresh_stream(), batch(count)),
                |(store, key, events)| async move {
                    store
                        .append(&key, events)
                        .await
                        .expect("an append to a stream of its own from sequence 1 must be taken");
                    store
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// What reading a whole stream costs, against three stream lengths, asked for
/// both ways a consumer asks.
///
/// `by_stream` is how an aggregate replays, and `from_global(1)` is how a
/// consumer that follows the journal in the order it accepted records starts.
///
/// Timed: the `load` call and pulling every record it serves.
///
/// Not timed: seeding. Each length gets a store of its own, filled once before
/// the window opens.
///
/// A store per length is what makes the two queries comparable at all: this
/// store copies everything it holds before it filters, so a `by_stream` load
/// out of a store that also held the other lengths would be charged for them.
fn load(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("in_memory_load");
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASUREMENT);

    for count in LOADED {
        let store = rt.block_on(seeded(count));
        let store = &store;
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("by_stream", count), &count, |b, _| {
            b.to_async(&rt)
                .iter(|| async move { drain(store, LoadQuery::by_stream(SEEDED_STREAM)).await });
        });
        group.bench_with_input(BenchmarkId::new("from_global", count), &count, |b, _| {
            b.to_async(&rt)
                .iter(|| async move { drain(store, LoadQuery::from_global(1)).await });
        });
    }

    group.finish();
}

criterion_group!(in_memory_event_store, append, load);
criterion_main!(in_memory_event_store);
