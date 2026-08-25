use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::SnapshotError;
use crate::id::AggregateId;
use crate::snapshot::PersistedSnapshot;
use crate::store::snapshot_store::SnapshotStore;

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
