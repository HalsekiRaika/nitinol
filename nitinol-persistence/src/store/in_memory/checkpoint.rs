use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::CheckpointError;
use crate::store::checkpoint_store::{CheckpointScope, CheckpointStore};

/// The rendered key is the whole of what a scope hands a store, so it is what
/// this one keys its map by: two consumers of different kinds are kept apart
/// here by exactly what keeps them apart in a backend's table.
pub struct InMemoryCheckpointStore {
    state: Mutex<HashMap<String, u64>>,
}

impl Default for InMemoryCheckpointStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
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
