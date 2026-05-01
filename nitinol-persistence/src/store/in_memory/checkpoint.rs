use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::CheckpointError;
use crate::id::ProjectionId;
use crate::store::checkpoint_store::CheckpointStore;

pub struct InMemoryCheckpointStore {
    state: Mutex<HashMap<ProjectionId, u64>>,
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

    async fn load(&self, projection_id: &ProjectionId) -> Result<Option<u64>, CheckpointError> {
        let state = self.state.lock().unwrap();
        Ok(state.get(projection_id).copied())
    }

    async fn save(
        &self,
        projection_id: &ProjectionId,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError> {
        let mut state = self.state.lock().unwrap();
        state.insert(projection_id.clone(), sequence);
        Ok(())
    }
}
