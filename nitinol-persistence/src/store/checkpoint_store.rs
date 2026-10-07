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
///
/// The clauses below are the contract every backend must reproduce.  They are
/// what a third-party implementation may rely on — the incidental behaviour of
/// any particular backend is not part of the interface — and they change only
/// as the crate's version does.  Each clause carries a stable label (`CKP-1` …
/// `CKP-6`).
///
/// Two executable records share those labels.
/// `nitinol-persistence/tests/checkpoint_store.rs` exercises
/// [`InMemoryCheckpointStore`](crate::store::InMemoryCheckpointStore) as the
/// reference implementation, and `nitinol-store-conformance`'s `checkpoint`
/// module is the suite any backend is put through, reference implementation
/// included.  A backend that has not been through it has not been shown to keep
/// this contract.  Two of the clauses the suite deliberately does not fix, and
/// each says so below.
///
/// # CKP-1: never started is not the same as at zero
///
/// `load` for a scope nothing has saved a position for answers `Ok(None)`.  Not
/// `Ok(Some(0))`.
///
/// A consumer reads this once to decide where to resume from, and a consumer
/// that really did stop at the store's first record is at zero.  A store
/// folding the two together — `COALESCE(sequence, 0)` is the usual way — leaves
/// nothing able to tell them apart.
///
/// Fixed by the suite's `a_scope_nothing_has_saved_for_has_no_position`.
///
/// # CKP-2: a saved position is what a load answers with
///
/// After `save(scope, n, ..)` is taken, `load(scope)` answers `Ok(Some(n))`.
///
/// Fixed by the suite's `a_saved_position_is_what_a_load_answers_with`.
///
/// # CKP-3: a move forward is taken however far it reaches
///
/// A `sequence` beyond the one the store holds for that scope is taken however
/// far beyond it reaches, and `load` then answers with it.  A store asks for no
/// particular next number.
///
/// Positions skip for ordinary reasons.  A projector following the store's
/// whole order moves over records it has no handler for, and one partition of a
/// delivery host counts the positions another partition finished as finished,
/// so what either of them saves is not the position before it plus one.  A
/// store requiring the next number would refuse every save they make.
///
/// Fixed by the suite's `a_move_forward_is_taken_however_far_it_reaches`.
///
/// # CKP-4: a save that is not a move forward is the backend's to answer
///
/// A `sequence` the store has already reached for that scope, or one behind it,
/// is **left to the backend**: refusing it with
/// [`CheckpointError::Backend`] and leaving the stored position where it was,
/// and taking it, are both conforming.
///
/// What makes that an open choice rather than a gap is that no correct consumer
/// makes such a save.  A projector judges each event against the checkpoint it
/// holds in memory and passes over anything at or below it; a delivery host
/// offers the store a position only where it is strictly ahead of what it last
/// recorded, and refuses a manual advance that would go backwards.  So a
/// non-forward save means a premise was already broken — something other than
/// the one owner of that key has been writing to it — and which of the two
/// answers serves an operator better depends on what the backend can see.
///
/// **Not fixed by a test**, and the suite deliberately never makes such a save.
/// The two permitted answers have no observation in common, and
/// [`CheckpointError`] carries no variant that distinguishes a refusal from a
/// backend failure, so a suite judging it would have to pick one of the two
/// answers and fail every backend giving the other.  A backend that refuses
/// owes a test of its own: `providers/nitinol-sqlx/tests/checkpoint_store.rs`
/// is where the Postgres provider's forward-only rule is fixed.
///
/// # CKP-5: the rendered key is the whole of what a store is told
///
/// A store files a position under the string
/// [`CheckpointScope::checkpoint_key`] rendered, and under nothing else:
///
/// * Scopes rendering strings that differ are kept apart.  A position saved
///   under one key is answered under that key alone, and a key nothing has
///   saved for still has no position while another holds one.  That holds while
///   several writers are recording at the same time, which is how a deployment
///   reaches a store: the partitions of a delivery host record from processes
///   of their own, under keys of their own, at the same moment.
/// * Two scopes rendering one string share one position, whatever their types.
///   This is what has a projection's rows found again after the framework grew
///   a second kind of consumer: [`ProjectionId`] renders verbatim, so a row
///   written when it was the only kind is read back under the same text.
/// * A key inside the [reserved namespace](crate::reserved) is stored like any
///   other.  A framework-owned consumer renders there on purpose — that is what
///   keeps its key from meeting a projection's — so a store reading the
///   namespace as something to validate would keep the framework's own
///   consumers from recording anything.
///
/// That the keys *cannot* collide is the identifier constructors' obligation
/// rather than a store's: every user-facing one refuses a reserved name, which
/// is why one table serves every kind.  That half is fixed by
/// `nitinol-saga/tests/saga_delivery_checkpoint_scope.rs` and
/// `nitinol-persistence/tests/reserved_namespace.rs`.
///
/// Fixed by the suite's `the_rendered_key_is_the_whole_of_what_a_store_is_told`
/// and, for the concurrent half,
/// `writers_under_keys_of_their_own_do_not_disturb_each_other`.
///
/// # CKP-6: a save handed a transaction belongs to it
///
/// Where `Tx` is a transaction of the backend's, a `save` handed one is part of
/// that transaction: it becomes visible when the caller commits, and it never
/// happened if the caller rolls back.  Where `Tx` is `()` there is nothing to
/// join, and the save stands on its own.
///
/// This is what [`DeliveryMode::ExactlyOnce`] with a `TxProvider` rests on — a
/// read-model update and the checkpoint covering it settle together or not at
/// all.
///
/// **Not fixed by the conformance suite.** The suite can make no backend's
/// transaction, and has nothing to hand a store whose `Tx` is `()`, so there is
/// no observation it could ask every backend for.  A backend whose `Tx` is a
/// transaction owes a test of its own:
/// `providers/nitinol-sqlx/tests/checkpoint_store.rs` fixes it for the Postgres
/// provider, commit and rollback both.
///
/// # What these clauses do not cover
///
/// Every clause above is one a backend keeps on its own.  Whether a
/// projection's result and the checkpoint covering it are settled in one
/// transaction, and whether a conflict takes back both, is a property of this
/// store *and* the machinery driving it — the projector and its `TxProvider` —
/// and it is fixed there rather than here:
/// `providers/nitinol-sqlx/tests/exactly_once_projection.rs` and
/// `nitinol-eventsource/tests/tx_wiring.rs` run a store and a projector at
/// once.  A store shown to keep CKP-1 … CKP-6 has not thereby been shown to be
/// driven correctly.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// The backend's own transaction, where it has one.  `()` where it has
    /// none; see CKP-6.
    type Tx: Send;

    /// The position `scope` has been carried as far as, or `Ok(None)` where
    /// nothing has saved one for it.
    ///
    /// CKP-1 is what makes the `None` usable: it means this consumer has never
    /// recorded, which a consumer at the store's first record is not.
    async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError>;

    /// Carry `scope`'s position to `sequence`, inside `tx` where one is handed
    /// over.
    ///
    /// CKP-2 and CKP-3 are what a caller may rely on: a position beyond the one
    /// the store holds is taken however far beyond it reaches, and a load then
    /// answers with it.  A `sequence` that is not a move forward is CKP-4's
    /// subject and the answer is the backend's.
    async fn save(
        &self,
        scope: &dyn CheckpointScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError>;
}

/// A checkpoint store reached through an `Arc` is that store, transaction type
/// included: erasing it would take away what a projector settling its update
/// and its position together needs to see.
#[async_trait]
impl<T: CheckpointStore + ?Sized> CheckpointStore for std::sync::Arc<T> {
    type Tx = T::Tx;

    async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError> {
        T::load(self, scope).await
    }

    async fn save(
        &self,
        scope: &dyn CheckpointScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError> {
        T::save(self, scope, sequence, tx).await
    }
}
