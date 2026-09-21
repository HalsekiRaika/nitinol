use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadQuery;

use crate::fixture;
use crate::under_test::{Executor, StoreUnderTest};

/// How many writers append to the store at the same time.
///
/// More than two, because the interleaving OCC-5 is about needs a writer to
/// number an event while another writer's commit is still in flight, and a
/// reader to be served past it in the meantime.
const WRITERS: usize = 4;

/// How many appends each writer makes.
const APPENDS_PER_WRITER: usize = 8;

/// Put a store under concurrent writers and a reader following the global
/// order, and check what that reader ends up having seen.
///
/// This is the stress half of the kit, and it is deliberately a separate entry
/// point from [`verify_deterministic`](crate::verify_deterministic): a pass
/// here says a run of this shape found nothing, which is a weaker thing than
/// the constructed evidence the deterministic clauses give, and the two must
/// not be mistaken for one another.
///
/// `executor` is the caller's runtime.  For a provider running this against a
/// real backend, the writers must reach the database independently — a pool
/// that serialises every task onto one connection makes this clause a
/// sequential test wearing a spawn, and it would pass against a store that
/// breaks OCC-5.
pub async fn verify_under_concurrency<S: StoreUnderTest, E: Executor>(store: &S, executor: &E) {
    a_checkpoint_reader_observes_every_committed_event_exactly_once(store, executor).await;
}

/// OCC-5. A reader that records the highest `global_sequence` it has observed
/// and asks only for what lies beyond it ends up having seen every event the
/// store committed, exactly once.
///
/// This is what the clause protects: nothing else notices when it is broken.
/// An event numbered before a commit that lands after a reader has already
/// passed that number is stored, is intact, and is read by nobody; no error is
/// raised at the writer, at the reader, or in between.
async fn a_checkpoint_reader_observes_every_committed_event_exactly_once<
    S: StoreUnderTest,
    E: Executor,
>(
    store: &S,
    executor: &E,
) {
    const CLAUSE: &str = "OCC-5";

    let backend = store.bring_up().await;
    let committed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let writers_left = Arc::new(AtomicUsize::new(WRITERS));

    let joins: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let backend = Arc::clone(&backend);
            let committed = Arc::clone(&committed);
            let writing = Writing(Arc::clone(&writers_left));
            executor.spawn(Box::pin(async move {
                // Held for the writer's whole life and given up on the way out,
                // whether that is the end of its work or a panic, so the reader
                // below is never left waiting on a writer that has died.
                let _writing = writing;
                append_a_stream_of_its_own(backend, committed, writer).await;
            }))
        })
        .collect();

    let mut checkpoint: Option<u64> = None;
    let mut observed: Vec<u64> = Vec::new();
    loop {
        // Read before the load, not after: a writer that finishes in between is
        // then accounted for by a load taken after its last append, where the
        // other order could stop on an emptiness that predates one.
        let still_writing = writers_left.load(Ordering::SeqCst) > 0;

        let query = match checkpoint {
            None => LoadQuery::default(),
            Some(observed_up_to) => LoadQuery::from_global(observed_up_to + 1),
        };
        let served = fixture::observe(&backend, query).await;
        for event in &served {
            observed.push(event.global_sequence);
            checkpoint = Some(match checkpoint {
                None => event.global_sequence,
                Some(observed_up_to) => observed_up_to.max(event.global_sequence),
            });
        }

        if !still_writing && served.is_empty() {
            break;
        }
        executor.yield_now().await;
    }

    for join in joins {
        join.await;
    }

    let committed = committed
        .lock()
        .expect("the suite's record of what was committed was poisoned by a panicking holder")
        .clone();
    let mut times_observed: HashMap<u64, usize> = HashMap::new();
    for global_sequence in &observed {
        *times_observed.entry(*global_sequence).or_insert(0) += 1;
    }

    let mut never_observed: Vec<u64> = committed
        .iter()
        .copied()
        .filter(|global_sequence| !times_observed.contains_key(global_sequence))
        .collect();
    never_observed.sort_unstable();
    assert!(
        never_observed.is_empty(),
        "{CLAUSE} is broken: {WRITERS} writers committed {} events and a reader following the \
         store's global order by checkpoint never observed the global_sequences \
         {never_observed:?}; a reader that has observed N asks only for what lies beyond N, so \
         an event that becomes visible at or below a checkpoint the reader has passed is read by \
         nobody and reported by nothing",
        committed.len()
    );

    let mut observed_twice: Vec<u64> = times_observed
        .iter()
        .filter(|(_, times)| **times > 1)
        .map(|(global_sequence, _)| *global_sequence)
        .collect();
    observed_twice.sort_unstable();
    assert!(
        observed_twice.is_empty(),
        "{CLAUSE} is broken: a reader that only ever asked for what lies beyond the highest \
         global_sequence it had observed was served {observed_twice:?} again afterwards"
    );
}

/// One writer's claim on the reader's attention, given up when it stops
/// writing.
struct Writing(Arc<AtomicUsize>);

impl Drop for Writing {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One writer, appending to a stream no other writer numbers.
///
/// Batches of one and of two alternate, so the commit unit the reader has to
/// keep up with is not always a single event.
async fn append_a_stream_of_its_own(
    backend: Arc<dyn EventStore>,
    committed: Arc<Mutex<Vec<u64>>>,
    writer: usize,
) {
    let key = format!("occ.conformance.concurrent.{writer}");
    let mut sequence = 0u64;

    for round in 0..APPENDS_PER_WRITER {
        let batch = if round % 2 == 0 { 1 } else { 2 };
        let events = (0..batch)
            .map(|_| {
                sequence += 1;
                fixture::fact(sequence, b"concurrent")
            })
            .collect();

        let outcome = match backend.append(&key, events).await {
            Ok(outcome) => outcome,
            Err(refusal) => panic!(
                "the suite cannot judge a store that refuses an append to {key:?}, a stream no \
                 other writer numbers: {refusal}"
            ),
        };
        committed
            .lock()
            .expect("the suite's record of what was committed was poisoned by a panicking holder")
            .extend(outcome.assigned_sequences);
    }
}
