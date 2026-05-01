use async_trait::async_trait;

use crate::error::SnapshotError;
use crate::id::AggregateId;
use crate::snapshot::PersistedSnapshot;

#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError>;

    async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError>;
}
