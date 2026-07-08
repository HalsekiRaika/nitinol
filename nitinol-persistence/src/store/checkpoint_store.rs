use async_trait::async_trait;

use crate::error::CheckpointError;
use crate::id::ProjectionId;

/// Delivery guarantee used by a projector when invoking
/// [`CheckpointStore::save`].
///
/// The variant controls **when** the checkpoint is persisted relative to the
/// projector's handler, which in turn determines whether a crash between
/// handler execution and checkpoint save causes the event to be skipped or
/// reprocessed. Choose the mode that matches the read-model's tolerance:
/// idempotent overwrites work with `AtLeastOnce`, non-idempotent counters
/// require `ExactlyOnce`, best-effort handling tolerates loss with
/// `AtMostOnce`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Checkpoint is saved **before** the handler runs. On crash mid-handler
    /// the event is not retried — at most one delivery, possibly zero.
    AtMostOnce,
    /// Checkpoint is saved **after** the handler returns successfully. A
    /// crash between the two causes the event to be redelivered on restart,
    /// so handlers must be idempotent.
    AtLeastOnce,
    /// Behavior depends on whether a `TxProvider` is configured on the
    /// projector.
    ///
    /// **Without a `TxProvider`**: the framework does **not** save the
    /// checkpoint. The user implementation inside `project()` is responsible
    /// for persisting both the read-model update and the checkpoint atomically.
    ///
    /// **With a `TxProvider` configured**: the framework coordinates the
    /// checkpoint save inside the transaction: for each event it calls
    /// `begin`, runs the handler (`project`), saves the checkpoint inside
    /// that transaction, then calls `commit`. If `project` or the checkpoint
    /// save fails, the transaction is rolled back and the event is not marked
    /// processed.
    ExactlyOnce,
}

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    type Tx: Send;

    async fn load(&self, projection_id: &ProjectionId) -> Result<Option<u64>, CheckpointError>;

    async fn save(
        &self,
        projection_id: &ProjectionId,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError>;
}
