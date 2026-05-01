use async_trait::async_trait;

use crate::error::CheckpointError;
use crate::id::ProjectionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// ユーザー実装次第で `()` または実 TX ハンドル
    type Tx: Send;

    async fn load(&self, projection_id: &ProjectionId) -> Result<Option<u64>, CheckpointError>;

    async fn save(
        &self,
        projection_id: &ProjectionId,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError>;
}
