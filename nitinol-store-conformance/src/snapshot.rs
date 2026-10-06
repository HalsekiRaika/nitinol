//! The contract every `SnapshotStore` owes, made executable for any backend.
//!
//! [`SnapshotStore`] states it as clauses `SNAP-1` … `SNAP-4`; this module is
//! those clauses as something to run.
//!
//! # One entry point, and why there is no second
//!
//! [`verify`] takes no executor and spawns nothing. The framework's only
//! consumer of a snapshot store drives every call through one process, so there
//! is no clause that needs a writer and a reader at the same time — and an
//! entry point for a concurrency half with no clause in it would suggest a kind
//! of evidence this contract does not have to give.
//!
//! # What a green run here is not
//!
//! Evidence about anything but the store alone. Whether an aggregate's
//! restored state and the events replayed after it add up to where that
//! aggregate had got to is a property of the store *and* the process driving
//! it; see the [crate documentation](crate) for where the combined contracts
//! are fixed.

use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use jiff::Timestamp;
use nitinol_persistence::store::SnapshotStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};

/// The snapshot backend a run of the `SNAP` clauses is about.
///
/// Every clause asks for its own store, so no clause can be carried by what
/// another one left behind, and a provider decides for itself what a store
/// holding nothing means for its backend — a fresh schema, a truncated table, a
/// pool of its own.
///
/// `Send + Sync` so that the future a clause returns is itself `Send`: a
/// provider running the suite as an integration test may want to spawn it.
pub trait SnapshotStoreUnderTest: Send + Sync {
    /// Hand the suite a store holding nothing.
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn SnapshotStore>>;
}

/// Put a snapshot store through every clause of the snapshot contract.
///
/// The clauses run in the order below, and the first one broken panics naming
/// its `SNAP` label.
pub async fn verify<S: SnapshotStoreUnderTest>(store: &S) {
    an_aggregate_with_no_snapshot_is_answered_rather_than_failed(store).await;
    one_save_reads_back_whole(store).await;
    the_save_that_came_last_is_what_a_load_answers_with(store).await;
    one_aggregates_snapshot_is_never_anothers(store).await;
}

/// SNAP-1. An aggregate the store holds no snapshot for is answered with
/// `Ok(None)`: a condition rather than a failure.
async fn an_aggregate_with_no_snapshot_is_answered_rather_than_failed<S: SnapshotStoreUnderTest>(
    store: &S,
) {
    const CLAUSE: &str = "SNAP-1";

    let backend = store.bring_up().await;
    let never_saved = AggregateId::new("snap.conformance.unsaved");

    absent(
        CLAUSE,
        &backend,
        &never_saved,
        "the store holds nothing at all",
    )
    .await;
}

/// SNAP-2. What one `save` wrote is what a load reads back, whole: the
/// aggregate it was for, the position it was taken at, and the state at that
/// position.
async fn one_save_reads_back_whole<S: SnapshotStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "SNAP-2";

    let backend = store.bring_up().await;
    let aggregate = AggregateId::new("snap.conformance.roundtrip");
    saved(CLAUSE, &backend, snapshot(&aggregate, 42, b"state-at-42")).await;

    let found = present(CLAUSE, &backend, &aggregate, "the one save it was handed").await;

    assert_eq!(
        found.aggregate_id, aggregate,
        "{CLAUSE} is broken: the snapshot saved for {aggregate:?} was read back as {:?}'s",
        found.aggregate_id
    );
    assert_eq!(
        found.sequence, 42,
        "{CLAUSE} is broken: the snapshot of {aggregate:?} was saved at sequence 42 and read back \
         at {}; the position is what the aggregate replays from, so a restored state and the \
         position it is replayed from have to be the pair one save wrote",
        found.sequence
    );
    assert_eq!(
        found.payload,
        Bytes::from_static(b"state-at-42"),
        "{CLAUSE} is broken: the payload saved for {aggregate:?} is not the payload read back"
    );
}

/// SNAP-3. Several saves for one aggregate, at ascending positions: a load
/// answers with the one that came last, position and state together.
///
/// The pair is the clause rather than the position alone. A store answering
/// with the latest position and an earlier state hands back something no save
/// ever wrote, and the aggregate then replays from beyond the events that state
/// is missing — the one reading of a snapshot that loses facts with no error
/// raised anywhere.
///
/// A save at a position the stored snapshot has already reached is left to the
/// backend and is not judged here; see [`SnapshotStore`] for why.
async fn the_save_that_came_last_is_what_a_load_answers_with<S: SnapshotStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "SNAP-3";

    let backend = store.bring_up().await;
    let aggregate = AggregateId::new("snap.conformance.latest");
    saved(CLAUSE, &backend, snapshot(&aggregate, 1, b"state-at-1")).await;
    saved(CLAUSE, &backend, snapshot(&aggregate, 5, b"state-at-5")).await;
    saved(CLAUSE, &backend, snapshot(&aggregate, 10, b"state-at-10")).await;

    let found = present(CLAUSE, &backend, &aggregate, "three saves it was handed").await;

    assert_eq!(
        found.sequence, 10,
        "{CLAUSE} is broken: the saves for {aggregate:?} reached sequence 10 and a load answered \
         with the one at {}; an aggregate restoring a superseded snapshot replays from the \
         position that snapshot was taken at",
        found.sequence
    );
    assert_eq!(
        found.payload,
        Bytes::from_static(b"state-at-10"),
        "{CLAUSE} is broken: a load for {aggregate:?} answered with the position of one save and \
         the state of another, which is a pair no save ever wrote; the aggregate would restore \
         that state and then replay from beyond the events it is missing"
    );
}

/// SNAP-4. Aggregates are kept apart: what was saved for one is read back for
/// it alone.
async fn one_aggregates_snapshot_is_never_anothers<S: SnapshotStoreUnderTest>(store: &S) {
    const CLAUSE: &str = "SNAP-4";

    let backend = store.bring_up().await;
    let mine = AggregateId::new("snap.conformance.isolation.mine");
    let anothers = AggregateId::new("snap.conformance.isolation.anothers");

    saved(CLAUSE, &backend, snapshot(&anothers, 7, b"anothers-state")).await;
    absent(
        CLAUSE,
        &backend,
        &mine,
        "the only save the store has taken was another aggregate's",
    )
    .await;

    saved(CLAUSE, &backend, snapshot(&mine, 3, b"my-state")).await;
    for (aggregate, sequence, state) in [
        (&mine, 3u64, b"my-state".as_slice()),
        (&anothers, 7, b"anothers-state".as_slice()),
    ] {
        let found = present(CLAUSE, &backend, aggregate, "a save of its own").await;
        assert_eq!(
            found.sequence, sequence,
            "{CLAUSE} is broken: {aggregate:?} was saved at sequence {sequence} and a load \
             answered with sequence {}, which is where the aggregate beside it is",
            found.sequence
        );
        assert_eq!(
            found.payload.as_ref(),
            state,
            "{CLAUSE} is broken: {aggregate:?} was saved with a state of its own and a load \
             answered with the other aggregate's"
        );
    }
}

// Staging and judging a clause

/// One snapshot to save, carrying `payload` so a clause can tell which save it
/// is reading back.
///
/// `created_at` is stamped because the type carries it, and no clause judges
/// it: no consumer in the framework reads it, so a backend's precision there is
/// its own business rather than something a third party may rely on.
fn snapshot(
    aggregate_id: &AggregateId,
    sequence: u64,
    payload: &'static [u8],
) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(payload),
        created_at: Timestamp::now(),
    }
}

/// A save a clause requires to succeed.
async fn saved(clause: &str, backend: &Arc<dyn SnapshotStore>, snapshot: PersistedSnapshot) {
    let aggregate_id = snapshot.aggregate_id.clone();
    let sequence = snapshot.sequence;
    backend.save(snapshot).await.unwrap_or_else(|failure| {
        panic!(
            "{clause} is broken: saving the snapshot of {aggregate_id:?} taken at sequence \
             {sequence} reported {failure}"
        )
    });
}

/// The snapshot a clause requires the store to answer with.
async fn present(
    clause: &str,
    backend: &Arc<dyn SnapshotStore>,
    aggregate_id: &AggregateId,
    holds: &str,
) -> PersistedSnapshot {
    backend
        .load_latest(aggregate_id)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: {aggregate_id:?} has {holds}, and loading its latest \
                 snapshot reported {failure}"
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "{clause} is broken: {aggregate_id:?} has {holds}, and a load answered with no \
                 snapshot at all"
            )
        })
}

/// A load a clause requires to answer that there is no snapshot — neither one
/// of somebody else's, nor a failure.
async fn absent(
    clause: &str,
    backend: &Arc<dyn SnapshotStore>,
    aggregate_id: &AggregateId,
    because: &str,
) {
    match backend.load_latest(aggregate_id).await {
        Ok(None) => {}
        Ok(Some(found)) => panic!(
            "{clause} is broken: nothing has been saved for {aggregate_id:?} — {because} — and a \
             load answered with the snapshot of {:?} taken at sequence {}",
            found.aggregate_id, found.sequence
        ),
        Err(failure) => panic!(
            "{clause} is broken: nothing has been saved for {aggregate_id:?} — {because} — which \
             is the condition every aggregate activating for the first time is in, and a load \
             reported {failure}; an aggregate cannot tell that from a broken store, and the \
             contract has it carry on from its default state"
        ),
    }
}
