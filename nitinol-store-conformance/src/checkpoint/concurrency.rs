use std::sync::Arc;

use nitinol_persistence::store::{CheckpointScope, CheckpointStore};

use super::fixture::{self, AConsumer};
use super::CheckpointStoreUnderTest;
use crate::under_test::Executor;

/// How many writers record at the same time.
///
/// Four, which is what a delivery host spread over four partitions does, and
/// few enough that a provider's pool can hold a connection for each — a writer
/// queued behind another's connection makes this a sequential run wearing a
/// spawn.
const WRITERS: usize = 4;

/// How many positions each writer records.
const SAVES_PER_WRITER: u64 = 8;

/// How far apart the writers' positions are kept.
///
/// Each writer's positions are a stretch of the number line of its own, so what
/// every key answers with at the end names the writer it belongs to. Ranges
/// that overlapped would have the highest position answer correctly for every
/// key, and a store with one slot for everything would pass.
const STRIDE: u64 = 100;

const _: () = assert!(
    STRIDE > SAVES_PER_WRITER,
    "a writer's positions must stay inside its own stretch of the number line, or what a key \
     answers with no longer names the writer it belongs to"
);

/// Put a checkpoint store under writers recording at the same time, and check
/// what each of their keys ends up holding.
///
/// This is the stress half of the checkpoint suite, and it is deliberately a
/// separate entry point from
/// [`verify_deterministic`](super::verify_deterministic): a pass here says a
/// run of this shape found nothing, which is a weaker thing than the
/// constructed evidence the deterministic clauses give, and the two must not be
/// mistaken for one another.
///
/// `executor` is the caller's runtime. For a provider running this against a
/// real backend, the writers must reach the database independently — a pool
/// that serialises every task onto one connection makes this a sequential test
/// wearing a spawn.
pub async fn verify_under_concurrency<S: CheckpointStoreUnderTest, E: Executor>(
    store: &S,
    executor: &E,
) {
    writers_under_keys_of_their_own_do_not_disturb_each_other(store, executor).await;
}

/// CKP-5, under concurrency. Several writers recording at once, each under a
/// key of its own: every key ends up holding the last position its own writer
/// saved.
///
/// This is how a deployment reaches a checkpoint store. The partitions of a
/// delivery host run as processes of their own and record their progress at the
/// same moment, and it is the one path that puts a provider's connection pool
/// under more than one writer at a time.
async fn writers_under_keys_of_their_own_do_not_disturb_each_other<
    S: CheckpointStoreUnderTest,
    E: Executor,
>(
    store: &S,
    executor: &E,
) {
    const CLAUSE: &str = "CKP-5";

    let backend = store.bring_up().await;

    let joins: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let backend = Arc::clone(&backend);
            executor.spawn(Box::pin(async move {
                record_under_a_key_of_its_own(backend, writer).await;
            }))
        })
        .collect();

    for join in joins {
        join.await;
    }

    for writer in 0..WRITERS {
        let consumer = key_of(writer);
        let finished_at = positions_of(writer)
            .last()
            .expect("a writer records at least one position");

        let answer = fixture::held(CLAUSE, &backend, &consumer).await;

        assert_eq!(
            answer,
            Some(finished_at),
            "{CLAUSE} is broken: {WRITERS} writers recorded under keys of their own at the same \
             time, and {:?} — whose writer finished at {finished_at} — answered with {answer:?}; \
             the rendered key is the whole of what a store is told, so a save under one key \
             reaches no other however the saves were interleaved",
            consumer.checkpoint_key()
        );
    }
}

/// One writer, recording under a key no other writer saves under.
///
/// Its positions ascend, as a correct consumer's do: a backend that takes none
/// but forward moves and one that overwrites whatever it is handed are both
/// conforming, so a writer that went backwards would be judging `CKP-4` — which
/// the contract leaves to the backend.
async fn record_under_a_key_of_its_own<S: CheckpointStore>(backend: Arc<S>, writer: usize) {
    let consumer = key_of(writer);

    for position in positions_of(writer) {
        if let Err(failure) = backend.save(&consumer, position, None).await {
            panic!(
                "the suite cannot judge a store that refuses the position {position} under {:?}, \
                 a key no other writer saves under and one this writer has not carried that far: \
                 {failure}",
                consumer.checkpoint_key()
            );
        }
    }
}

/// The key one writer records under.
fn key_of(writer: usize) -> AConsumer {
    AConsumer::rendering(format!("ckp.conformance.concurrent.{writer}"))
}

/// The positions one writer records, in the order it records them.
fn positions_of(writer: usize) -> impl Iterator<Item = u64> {
    let base = writer as u64 * STRIDE;
    (1..=SAVES_PER_WRITER).map(move |step| base + step)
}
