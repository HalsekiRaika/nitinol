use async_trait::async_trait;

use crate::error::SnapshotError;
use crate::id::AggregateId;
use crate::snapshot::PersistedSnapshot;

/// Where an aggregate's state at a position is kept, so activating it does not
/// have to start from its first event.
///
/// A snapshot is a shortcut and never a record: the aggregate's events are, so
/// a snapshot can be rebuilt by replaying from the beginning, and losing one
/// costs a longer replay rather than a fact.  What it saves is only worth
/// anything if what comes back is the state the aggregate had *at the position
/// the snapshot names* — an aggregate restores the state and then replays the
/// events above that position, so a state and a position that did not belong
/// together lose every event in between with no error raised anywhere.
///
/// The clauses below are the contract every backend must reproduce.  They are
/// what a third-party implementation may rely on — the incidental behaviour of
/// any particular backend is not part of the interface — and they change only
/// as the crate's version does.  Each clause carries a stable label (`SNAP-1` …
/// `SNAP-4`).
///
/// Two executable records share those labels.
/// `nitinol-persistence/tests/snapshot_store.rs` exercises
/// [`InMemorySnapshotStore`](crate::store::InMemorySnapshotStore) as the
/// reference implementation, and `nitinol-store-conformance`'s `snapshot`
/// module is the suite any backend is put through, reference implementation
/// included.  A backend that has not been through it has not been shown to keep
/// this contract.
///
/// # SNAP-1: nothing saved is a condition, not a failure
///
/// `load_latest` for an aggregate the store holds no snapshot for answers
/// `Ok(None)`.  Not an error, and not a snapshot belonging to somebody else.
///
/// Every aggregate activating for the first time is in that state, and the
/// contract has it carry on from its default state and replay from its first
/// event.  An error there cannot be told from a store that is broken.
///
/// Fixed by the suite's `an_aggregate_with_no_snapshot_is_answered_rather_than_failed`.
///
/// # SNAP-2: one save reads back whole
///
/// What one `save` wrote is what `load_latest` reads back: the
/// `aggregate_id` it was for, the `sequence` it was taken at, and the `payload`
/// holding the state at that position.
///
/// The three are one save's, together.  A `sequence` from one save beside a
/// `payload` from another is a state no save ever wrote, and it is the worse
/// half of SNAP-3 for that reason.
///
/// `created_at` is stored as it was handed over and read back, and is **not**
/// fixed by a test: no consumer in the framework reads it, so what precision a
/// backend keeps it to is its own business rather than something a third party
/// may rely on.
///
/// Fixed by the suite's `one_save_reads_back_whole`.
///
/// # SNAP-3: the save that came last is what a load answers with
///
/// Several saves for one aggregate at ascending `sequence`es: `load_latest`
/// answers with the one that came last — which is the one at the highest
/// position — and answers with its `sequence` and its `payload` together.
///
/// A save at a `sequence` the stored snapshot has already reached, or behind
/// it, is **left to the backend** and is not fixed by a test.  A single writer
/// takes snapshots at positions that only grow, and the framework's only
/// consumer never makes such a save, so the two answers a backend could give —
/// taking it, or refusing it — are both consistent with every consumer there
/// is.  The reference implementation keeps whichever save came last.
///
/// Fixed by the suite's `the_save_that_came_last_is_what_a_load_answers_with`,
/// which judges the position and the state together: a store answering with the
/// latest position and an earlier state would pass a clause that checked the
/// position alone.
///
/// # SNAP-4: one aggregate's snapshot is never another's
///
/// A save for one aggregate changes what `load_latest` answers for that
/// aggregate and for no other.  An aggregate the store holds nothing for is
/// still answered `Ok(None)` while it holds another's, and once both have one
/// each load answers with its own.
///
/// Fixed by the suite's `one_aggregates_snapshot_is_never_anothers`.
///
/// # What these clauses do not cover
///
/// Every clause above is one a backend keeps on its own.  Whether an
/// aggregate's restored state and the events replayed above it add up to where
/// that aggregate had got to is a property of this store *and* the process
/// driving it, and it is fixed there rather than here:
/// `nitinol-eventsource/tests/e2e_snapshot.rs` runs the restore-then-replay
/// path.  A store shown to keep SNAP-1 … SNAP-4 has not thereby been shown to
/// be driven correctly.
#[async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Stores `snapshot` as the state of its aggregate at its `sequence`.
    ///
    /// SNAP-2 and SNAP-3 are what a caller may rely on here: the save reads
    /// back whole, and the one that came last is what a load answers with.
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError>;

    /// The snapshot `aggregate_id` saved last, or `Ok(None)` where it has saved
    /// none.
    ///
    /// SNAP-1 is what makes the `None` usable: it means this aggregate has
    /// never been snapshotted, which is a condition rather than a failure.
    async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError>;
}
