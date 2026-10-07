use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::CheckpointError;
use crate::store::checkpoint_store::{CheckpointScope, CheckpointStore};

/// In-memory reference implementation of [`CheckpointStore`].
///
/// This type is the reference implementation of the checkpoint contract every
/// backend must reproduce: never started is not the same as at zero, a saved
/// position is what a load answers with, a move forward is taken however far it
/// reaches, and the rendered key is the whole of what a store is told.  The
/// clauses, their labels and their exact wording live on [`CheckpointStore`].
///
/// The rendered key is the whole of what a scope hands a store, so it is what
/// this one keys its map by: two consumers of different kinds are kept apart
/// here by exactly what keeps them apart in a backend's table.
///
/// A save is taken whatever position it carries.  That is this type's answer to
/// the one thing the contract leaves to a backend — a save that would not carry
/// a checkpoint forward — rather than something a third party may rely on; the
/// Postgres provider refuses such a save instead.  `Tx` is `()` because there
/// is no transaction to join here.
///
/// The contract is fixed by two executable records, neither of which is this
/// implementation's incidental behaviour: the tests in
/// `nitinol-persistence/tests/checkpoint_store.rs`, which run against this
/// type, and `nitinol-store-conformance`, the suite a third-party backend is
/// put through — this type among them.
///
/// Intended for tests and examples; not for production use.
///
/// # A handle, not a region
///
/// Cloning this names the same positions, and [`Default`] constructs a
/// destination of its own — which is what naming one handle into both
/// checkpoint slots of a store means, and what naming two apart means.
#[derive(Clone)]
pub struct InMemoryCheckpointStore {
    state: Arc<Mutex<HashMap<String, u64>>>,
}

impl Default for InMemoryCheckpointStore {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    type Tx = ();

    async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError> {
        let state = self
            .state
            .lock()
            .expect("in-memory checkpoint state lock was poisoned by a panicking holder");
        Ok(state.get(scope.checkpoint_key()).copied())
    }

    async fn save(
        &self,
        scope: &dyn CheckpointScope,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError> {
        let mut state = self
            .state
            .lock()
            .expect("in-memory checkpoint state lock was poisoned by a panicking holder");
        state.insert(scope.checkpoint_key().to_owned(), sequence);
        Ok(())
    }
}
