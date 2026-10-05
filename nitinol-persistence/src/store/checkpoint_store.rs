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
    ///
    /// **What this guarantees, and what it does not.** The guarantee above
    /// holds for a single consumer per [`ProjectionId`] — one process, or one
    /// holding an exclusive claim on that projection, driving it. A projector
    /// loads its checkpoint once at startup and judges each event against the
    /// copy it holds in memory from then on, so two projectors starting from
    /// the same stored checkpoint each read the same event as unprocessed and
    /// can each commit: atomicity with its own checkpoint stops neither.
    /// Acquiring and revalidating the right to process a projection, and
    /// ordering events between concurrent projectors sharing a
    /// `projection_id`, are out of scope here and remain a future design
    /// target.
    ExactlyOnce,
}

/// What a consumer of events is called where its progress is kept.
///
/// A [`CheckpointStore`] holds the positions of more than one kind of consumer
/// — a projection, a partition of a delivery host — and each kind names itself
/// with its own type. This trait is the whole of what a store is told: the key
/// the position is filed under.
///
/// # Keys that cannot meet
///
/// Two kinds of consumer sharing one table must not be able to render one key,
/// or one would read the other's position as its own. The rendering is
/// therefore a property of the implementing type rather than of a convention
/// the caller follows:
///
/// - [`ProjectionId`] renders **verbatim**, so a row written before any other
///   kind of consumer existed is still found under the text its identifier
///   carries.
/// - A framework-owned consumer renders inside the
///   [reserved namespace](crate::reserved), which every user-facing identifier
///   constructor refuses. A [`ProjectionId`] that could collide with one cannot
///   be constructed at all.
///
/// Sharing the table is what is left once the keys cannot collide: the store
/// keeps one kind of row, and what distinguishes the consumers is the key.
pub trait CheckpointScope: Send + Sync {
    /// The key this consumer's position is filed under.
    fn checkpoint_key(&self) -> &str;
}

/// A projection is named by its identifier, with nothing added to it.
///
/// Decorating the key — a prefix naming the kind, say — would move every row a
/// projection had already written.
impl CheckpointScope for ProjectionId {
    fn checkpoint_key(&self) -> &str {
        self.as_str()
    }
}

/// The persisted progress cache every event consumer shares.
///
/// What a checkpoint holds is where a consumer had got to, and nothing more.
/// The business record is the events themselves — the aggregates' and the
/// sagas' — so a checkpoint can be rebuilt by reading from the beginning, and
/// losing one costs redelivery rather than a fact.
///
/// The key is a [`CheckpointScope`] rather than any one consumer's identifier,
/// so one store serves every kind of consumer without either kind knowing the
/// other exists.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    type Tx: Send;

    async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError>;

    async fn save(
        &self,
        scope: &dyn CheckpointScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError>;
}
