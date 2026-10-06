use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::SnapshotError;
use crate::id::AggregateId;
use crate::snapshot::PersistedSnapshot;
use crate::store::snapshot_store::SnapshotStore;

/// In-memory reference implementation of [`SnapshotStore`].
///
/// This type is the reference implementation of the snapshot contract every
/// backend must reproduce: nothing saved is a condition rather than a failure,
/// one save reads back whole, the save that came last is what a load answers
/// with, and one aggregate's snapshot is never another's.  The clauses, their
/// labels and their exact wording live on [`SnapshotStore`].
///
/// One save per aggregate is kept, so a later save replaces an earlier one.
/// That is this type's answer to the one thing the contract leaves to a backend
/// — a save at a position the stored snapshot has already reached — rather than
/// a clause a third party may rely on.
///
/// The contract is fixed by two executable records, neither of which is this
/// implementation's incidental behaviour: the tests in
/// `nitinol-persistence/tests/snapshot_store.rs`, which run against this type,
/// and `nitinol-store-conformance`, the suite a third-party backend is put
/// through — this type among them.
///
/// Intended for tests and examples; not for production use.
pub struct InMemorySnapshotStore {
    state: Mutex<HashMap<AggregateId, PersistedSnapshot>>,
}

impl Default for InMemorySnapshotStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SnapshotStore for InMemorySnapshotStore {
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        let mut state = self
            .state
            .lock()
            .expect("in-memory snapshot state lock was poisoned by a panicking holder");
        state.insert(snapshot.aggregate_id.clone(), snapshot);
        Ok(())
    }

    async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        let state = self
            .state
            .lock()
            .expect("in-memory snapshot state lock was poisoned by a panicking holder");
        Ok(state.get(aggregate_id).cloned())
    }
}
