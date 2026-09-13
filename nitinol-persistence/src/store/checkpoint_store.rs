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
    ///
    /// A handler that returns an error is *not* retried either: the checkpoint
    /// was already persisted, and the subscription moves on to the next event.
    /// That is the loss this mode is chosen for.
    AtMostOnce,
    /// Checkpoint is saved **after** the handler returns successfully. A
    /// crash between the two causes the event to be redelivered on restart,
    /// so handlers must be idempotent.
    ///
    /// A handler that returns an error, or a checkpoint save that fails, leaves
    /// the event unprocessed: the subscription does not move past it, and it is
    /// redelivered on the next poll rather than overtaken by the events behind
    /// it. An event that keeps failing therefore keeps everything behind it
    /// waiting.
    AtLeastOnce,
    /// Behavior depends on whether a `TxProvider` is configured on the
    /// projector.
    ///
    /// **Without a `TxProvider`**: the framework does **not** save the
    /// checkpoint. The user implementation inside `project()` is responsible
    /// for persisting both the read-model update and the checkpoint atomically.
    /// Having no checkpoint of its own to compare, the framework takes the
    /// `Result` of `project()` as the whole account of what happened: `Ok`
    /// means processed and the subscription moves on, `Err` means the event was
    /// not processed and the subscription holds it for redelivery. A `project()`
    /// that committed its work and then failed for an unrelated reason will be
    /// called again with the same event, so it must be prepared to recognise
    /// what it already wrote.
    ///
    /// **With a `TxProvider` configured**: the framework coordinates the
    /// checkpoint save inside the transaction: for each event it calls
    /// `begin`, runs the handler (`project`), saves the checkpoint inside
    /// that transaction, then calls `commit`. If `begin`, `project`, the
    /// checkpoint save or `commit` fails, the event is not marked processed:
    /// any started transaction is rolled back and the subscription holds the
    /// event for redelivery. A failed `commit` is read as uncommitted — see the
    /// `TxProvider` trait in `nitinol-eventsource` for why that reading is the
    /// safe one and what it costs.
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
