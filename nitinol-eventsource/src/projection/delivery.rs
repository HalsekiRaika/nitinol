use std::future::Future;
use std::pin::Pin;

use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};

use crate::projection::ProjectionId;

use crate::projection::context::ProjectionContext;
use crate::projection::tx_provider::ErasedTxProvider;

/// What a handler answers for one record.
pub(crate) type ProjectResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A handler's run over one record, borrowing the projector and the context it
/// was handed for as long as it runs.
pub(crate) type ProjectFuture<'a> = Pin<Box<dyn Future<Output = ProjectResult> + Send + 'a>>;

/// A handler is only lent the transaction — `ProjectionContext::tx` hands out
/// `&mut`, and taking it back out is this crate's alone — so its context still
/// holds it when the handler returns.
const TX_STAYS_IN_CONTEXT: &str =
    "a handler is lent the transaction, so its context still holds it afterwards";

/// How far one batch settled, for the subscription cursor and the offset alike.
///
/// The answer cannot be recovered from the offset store: a mode that saves
/// nothing, a batch of skips that saves nothing, and a batch whose transaction
/// was rolled back all leave the stored offset where it was. Only
/// [`apply_batch`] knows which delivery mode ran and which stage of it failed,
/// so it states how far the batch settled as one value, and its caller projects
/// both the offset it carries forward and the answer the poller reads from that
/// one value rather than re-deciding either.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BatchOutcome {
    /// Every record up to and including `through` is settled — applied,
    /// skipped, or declared delivered by the mode in force. A batch that
    /// settled whole is settled through its last observed position.
    Settled { through: u64 },
    /// Nothing in the batch settled. Holding the cursor here is what gets the
    /// batch redelivered from its first record instead of skipped.
    Unsettled,
}

/// One record of a batch, as the projector classified it before delivery.
pub(crate) enum BatchRecord<I> {
    /// Nothing is owed for this record — it was applied by an earlier
    /// incarnation, or no handler follows its type — so it settles without a
    /// handler running.
    Skipped { position: u64 },
    /// `item` is handed to a handler, whose context reports `position` as the
    /// record's `current_sequence`.
    ToProject { position: u64, item: I },
}

impl<I> BatchRecord<I> {
    fn position(&self) -> u64 {
        match self {
            Self::Skipped { position } | Self::ToProject { position, .. } => *position,
        }
    }
}

/// What one read of the journal returned, classified record by record.
pub(crate) struct Batch<I> {
    /// In ascending position order.
    pub(crate) records: Vec<BatchRecord<I>>,
    /// The position of the last record the poller read: where a batch that
    /// settled whole leaves the offset.
    pub(crate) last_observed: u64,
}

impl<I> Batch<I> {
    fn has_records_to_project(&self) -> bool {
        self.records
            .iter()
            .any(|record| matches!(record, BatchRecord::ToProject { .. }))
    }
}

/// What one projection is called, in the two shapes applying an event needs.
///
/// The identifier is what a handler is given through its
/// [`ProjectionContext`]; the scope is the key the store files the position
/// under. They are one projection's name in two renderings, built together when
/// the projector is spawned and held together for as long as it runs, so they
/// travel as one here rather than as two parameters a caller could pair wrongly.
#[derive(Clone, Copy)]
pub(crate) struct ProjectionIdentity<'a> {
    pub(crate) id: &'a ProjectionId,
    pub(crate) scope: &'a OffsetScope,
}

/// Apply `batch` under `mode`, handing each record to be projected to
/// `project` in order, and say how far it settled.
///
/// The offset is saved at most once per batch, and with a `TxProvider` under
/// `ExactlyOnce` the whole batch is one transaction. A batch with nothing to
/// project settles whole without opening a transaction or saving an offset:
/// there is no work to make durable, and an empty transaction per poll would
/// be a commit spent on nothing.
///
/// `project` is handed the projector on every call rather than capturing it, so
/// that each call can borrow it for the handler's run and give it back.
pub(crate) async fn apply_batch<'pid, Cs, P, I, F>(
    mode: DeliveryMode,
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    tx_provider: Option<&(dyn ErasedTxProvider<Cs::Tx> + Send + Sync)>,
    projector: &mut P,
    project: F,
) -> BatchOutcome
where
    Cs: OffsetStore,
    Cs::Tx: 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Cs::Tx>) -> ProjectFuture<'a>
        + Send,
{
    if !batch.has_records_to_project() {
        return BatchOutcome::Settled {
            through: batch.last_observed,
        };
    }
    match tx_provider {
        Some(provider) if mode == DeliveryMode::ExactlyOnce => {
            apply_in_tx(store, projection, batch, provider, projector, project).await
        }
        _ => match mode {
            DeliveryMode::AtMostOnce => {
                apply_at_most_once(store, projection, batch, projector, project).await
            }
            DeliveryMode::AtLeastOnce => {
                apply_at_least_once(store, projection, batch, projector, project).await
            }
            DeliveryMode::ExactlyOnce => {
                apply_exactly_once_without_tx(projection, batch, projector, project).await
            }
        },
    }
}

/// `begin`, every handler inside the one transaction, the offset saved at the
/// batch's end inside it, `commit`. A failure at any stage leaves the whole
/// batch unapplied: nothing a rolled-back transaction carried can count as
/// settled for any record of it.
async fn apply_in_tx<'pid, Cs, P, I, F>(
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    provider: &(dyn ErasedTxProvider<Cs::Tx> + Send + Sync),
    projector: &mut P,
    mut project: F,
) -> BatchOutcome
where
    Cs: OffsetStore,
    Cs::Tx: 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Cs::Tx>) -> ProjectFuture<'a>
        + Send,
{
    let last_observed = batch.last_observed;
    let mut tx = match ErasedTxProvider::begin(provider).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                error = %e,
                last_observed,
                "TxProvider::begin failed; batch not applied and will be redelivered",
            );
            return BatchOutcome::Unsettled;
        }
    };

    for record in batch.records {
        let BatchRecord::ToProject { position, item } = record else {
            continue;
        };
        let mut ctx =
            ProjectionContext::new(projection.id, position, DeliveryMode::ExactlyOnce, Some(tx));
        let result = project(&mut *projector, item, &mut ctx).await;
        tx = ctx.take_tx().expect(TX_STAYS_IN_CONTEXT);
        if let Err(e) = result {
            tracing::warn!(
                error = %e,
                sequence = position,
                "project() failed (exactly-once + tx); rolling the batch back",
            );
            ErasedTxProvider::rollback(provider, tx).await;
            return BatchOutcome::Unsettled;
        }
    }

    if let Err(e) = store
        .save(projection.scope, last_observed, Some(&mut tx))
        .await
    {
        tracing::error!(
            error = %e,
            last_observed,
            "offset save failed (exactly-once + tx); rolling the batch back",
        );
        ErasedTxProvider::rollback(provider, tx).await;
        return BatchOutcome::Unsettled;
    }

    if let Err(e) = ErasedTxProvider::commit(provider, tx).await {
        // `commit` consumed the transaction and answers with a single `Err`,
        // so a lost reply from a backend that did commit is indistinguishable
        // here from one that did not. Reading it as uncommitted risks a
        // duplicate application; reading it as committed loses the batch
        // outright.
        tracing::error!(
            error = %e,
            last_observed,
            "TxProvider::commit failed; treating the batch as uncommitted \
             and redelivering it",
        );
        return BatchOutcome::Unsettled;
    }

    BatchOutcome::Settled {
        through: last_observed,
    }
}

/// The batch's end is saved before any handler runs, and a handler that fails
/// is left behind: every record of the batch is spent either way — that is the
/// guarantee the mode names.
async fn apply_at_most_once<'pid, Cs, P, I, F>(
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    projector: &mut P,
    mut project: F,
) -> BatchOutcome
where
    Cs: OffsetStore,
    Cs::Tx: 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Cs::Tx>) -> ProjectFuture<'a>
        + Send,
{
    let last_observed = batch.last_observed;
    if let Err(e) = store.save(projection.scope, last_observed, None).await {
        tracing::error!(
            error = %e,
            last_observed,
            "offset save failed before project (at-most-once)",
        );
    }

    for record in batch.records {
        let BatchRecord::ToProject { position, item } = record else {
            continue;
        };
        let mut ctx =
            ProjectionContext::new(projection.id, position, DeliveryMode::AtMostOnce, None);
        if let Err(e) = project(&mut *projector, item, &mut ctx).await {
            tracing::warn!(
                error = %e,
                sequence = position,
                "project() failed (at-most-once); event skipped",
            );
        }
    }

    BatchOutcome::Settled {
        through: last_observed,
    }
}

/// The records before the first failure are settled, and the offset is saved
/// once, at the last of them, after their handlers succeeded. The failing
/// record and everything behind it are redelivered.
async fn apply_at_least_once<'pid, Cs, P, I, F>(
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    projector: &mut P,
    project: F,
) -> BatchOutcome
where
    Cs: OffsetStore,
    Cs::Tx: 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Cs::Tx>) -> ProjectFuture<'a>
        + Send,
{
    let run = project_until_failure(
        DeliveryMode::AtLeastOnce,
        projection,
        batch,
        projector,
        project,
    )
    .await;
    let Some(through) = run.settled_through else {
        return BatchOutcome::Unsettled;
    };
    // Skips alone leave nothing a restart would redo, so they are not worth a
    // save of their own; the next batch that applies something carries the
    // offset past them.
    if !run.applied_any {
        return BatchOutcome::Settled { through };
    }
    match store.save(projection.scope, through, None).await {
        Ok(()) => BatchOutcome::Settled { through },
        Err(e) => {
            tracing::error!(
                error = %e,
                through,
                "offset save failed after project (at-least-once); \
                 batch will be redelivered",
            );
            BatchOutcome::Unsettled
        }
    }
}

/// Without a `TxProvider` the framework saves no offset, so each handler's
/// `Result` is the only account it has of what happened. Taking a
/// self-reported failure as processed is the one reading that contradicts the
/// mode's own guarantee, so the batch settles up to the record before it.
async fn apply_exactly_once_without_tx<'pid, Tx, P, I, F>(
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    projector: &mut P,
    project: F,
) -> BatchOutcome
where
    Tx: Send + 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Tx>) -> ProjectFuture<'a> + Send,
{
    let run = project_until_failure(
        DeliveryMode::ExactlyOnce,
        projection,
        batch,
        projector,
        project,
    )
    .await;
    match run.settled_through {
        Some(through) => BatchOutcome::Settled { through },
        None => BatchOutcome::Unsettled,
    }
}

/// How far an in-order run over a batch got before its first failing handler.
struct Run {
    /// The batch's last observed position if no handler failed; otherwise the
    /// position of the last record before the failure, or `None` when the
    /// failure was the batch's first record.
    settled_through: Option<u64>,
    /// Whether any handler returned `Ok`.
    applied_any: bool,
}

async fn project_until_failure<'pid, Tx, P, I, F>(
    mode: DeliveryMode,
    projection: ProjectionIdentity<'pid>,
    batch: Batch<I>,
    projector: &mut P,
    mut project: F,
) -> Run
where
    Tx: Send + 'static,
    P: Send,
    I: Send,
    F: for<'a> FnMut(&'a mut P, I, &'a mut ProjectionContext<'pid, Tx>) -> ProjectFuture<'a> + Send,
{
    let mut settled_through = None;
    let mut applied_any = false;
    for record in batch.records {
        let position = record.position();
        if let BatchRecord::ToProject { item, .. } = record {
            let mut ctx = ProjectionContext::new(projection.id, position, mode, None);
            if let Err(e) = project(&mut *projector, item, &mut ctx).await {
                tracing::warn!(
                    error = %e,
                    sequence = position,
                    delivery_mode = ?mode,
                    "project() failed; batch settled up to the record before it, \
                     which is redelivered",
                );
                return Run {
                    settled_through,
                    applied_any,
                };
            }
            applied_any = true;
        }
        settled_through = Some(position);
    }
    Run {
        settled_through: Some(batch.last_observed),
        applied_any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::projection::ProjectionId;
    use async_trait::async_trait;
    use nitinol_persistence::error::OffsetError;
    use nitinol_persistence::store::{InMemoryOffsetStore, OffsetScope};

    /// The position a one-record batch is at. Non-zero, so a
    /// `Settled` that wrongly reported something other than the record's own
    /// position is visible in the value as well as in the variant.
    const SEQUENCE: u64 = 9;

    fn failure() -> Box<dyn std::error::Error + Send + Sync> {
        Box::<dyn std::error::Error + Send + Sync>::from("intentional failure")
    }

    /// A `OffsetStore` that can be told to refuse every save, so the
    /// "handler succeeded but the offset did not land" stage is reachable.
    struct TestOffsetStore {
        inner: InMemoryOffsetStore,
        fail_save: bool,
        saves: AtomicU64,
    }

    impl TestOffsetStore {
        fn new(fail_save: bool) -> Self {
            Self {
                inner: InMemoryOffsetStore::default(),
                fail_save,
                saves: AtomicU64::new(0),
            }
        }

        fn saves(&self) -> u64 {
            self.saves.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OffsetStore for TestOffsetStore {
        type Tx = ();

        async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
            self.inner.load(scope).await
        }

        async fn save(
            &self,
            scope: &OffsetScope,
            sequence: u64,
            tx: Option<&mut Self::Tx>,
        ) -> Result<(), OffsetError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            if self.fail_save {
                return Err(OffsetError::Backend(failure()));
            }
            self.inner.save(scope, sequence, tx).await
        }
    }

    /// A transaction provider that can be told to fail `begin` or `commit`, and
    /// that counts what the delivery engine asked of it.
    struct TestTxProvider {
        fail_begin: bool,
        fail_commit: bool,
        begun: AtomicU64,
        committed: AtomicU64,
        rolled_back: AtomicU64,
    }

    impl TestTxProvider {
        fn new(fail_begin: bool, fail_commit: bool) -> Self {
            Self {
                fail_begin,
                fail_commit,
                begun: AtomicU64::new(0),
                committed: AtomicU64::new(0),
                rolled_back: AtomicU64::new(0),
            }
        }

        fn succeeding() -> Self {
            Self::new(false, false)
        }

        fn begun(&self) -> u64 {
            self.begun.load(Ordering::SeqCst)
        }

        fn committed(&self) -> u64 {
            self.committed.load(Ordering::SeqCst)
        }

        fn rolled_back(&self) -> u64 {
            self.rolled_back.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ErasedTxProvider<()> for TestTxProvider {
        async fn begin(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.begun.fetch_add(1, Ordering::SeqCst);
            if self.fail_begin {
                return Err(failure());
            }
            Ok(())
        }

        async fn commit(&self, _tx: ()) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.committed.fetch_add(1, Ordering::SeqCst);
            if self.fail_commit {
                return Err(failure());
            }
            Ok(())
        }

        async fn rollback(&self, _tx: ()) {
            self.rolled_back.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A record whose handler answers `Ok` when `succeeds`.
    fn to_project(position: u64, succeeds: bool) -> BatchRecord<bool> {
        BatchRecord::ToProject {
            position,
            item: succeeds,
        }
    }

    fn skipped(position: u64) -> BatchRecord<bool> {
        BatchRecord::Skipped { position }
    }

    fn one_record(succeeds: bool) -> Batch<bool> {
        Batch {
            records: vec![to_project(SEQUENCE, succeeds)],
            last_observed: SEQUENCE,
        }
    }

    /// The handler every case runs: note the position it was called for, and
    /// answer as the record says.
    fn note_and_answer<'a>(
        called_for: &'a mut Vec<u64>,
        succeeds: bool,
        ctx: &'a mut ProjectionContext<'_, ()>,
    ) -> ProjectFuture<'a> {
        called_for.push(ctx.current_sequence());
        Box::pin(async move {
            if succeeds {
                Ok(())
            } else {
                Err(failure())
            }
        })
    }

    /// One projection and the store it saves into.
    struct Case {
        id: ProjectionId,
        scope: OffsetScope,
        store: TestOffsetStore,
    }

    impl Case {
        fn new(name: &str) -> Self {
            Self::with_store(name, TestOffsetStore::new(false))
        }

        fn refusing_saves(name: &str) -> Self {
            Self::with_store(name, TestOffsetStore::new(true))
        }

        fn with_store(name: &str, store: TestOffsetStore) -> Self {
            let id = ProjectionId::new(name);
            let scope = OffsetScope::from(&id);
            Self { id, scope, store }
        }

        /// Apply `batch` and answer its outcome and the positions handlers
        /// were called for, in call order.
        async fn apply(
            &self,
            mode: DeliveryMode,
            provider: Option<&TestTxProvider>,
            batch: Batch<bool>,
        ) -> (BatchOutcome, Vec<u64>) {
            let mut called_for = Vec::new();
            let outcome = apply_batch(
                mode,
                &self.store,
                ProjectionIdentity {
                    id: &self.id,
                    scope: &self.scope,
                },
                batch,
                provider.map(|p| p as &(dyn ErasedTxProvider<()> + Send + Sync)),
                &mut called_for,
                note_and_answer,
            )
            .await;
            (outcome, called_for)
        }

        async fn stored(&self) -> Option<u64> {
            self.store.load(&self.scope).await.expect("load ok")
        }
    }

    // AtMostOnce: the batch's end is spent before the handlers run, so every
    // outcome settles the whole batch.

    #[tokio::test]
    async fn at_most_once_settles_on_success() {
        let case = Case::new("amo-ok");

        let (outcome, _) = case
            .apply(DeliveryMode::AtMostOnce, None, one_record(true))
            .await;

        assert_eq!(outcome, BatchOutcome::Settled { through: SEQUENCE });
        assert_eq!(case.stored().await, Some(SEQUENCE));
    }

    #[tokio::test]
    async fn at_most_once_settles_on_project_failure_after_saving_the_offset() {
        let case = Case::new("amo-fail");

        let (outcome, _) = case
            .apply(DeliveryMode::AtMostOnce, None, one_record(false))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Settled { through: SEQUENCE },
            "AtMostOnce trades a failed handler away; it must not ask for a retry"
        );
        assert_eq!(
            case.stored().await,
            Some(SEQUENCE),
            "AtMostOnce must have saved the offset before the (failed) project"
        );
    }

    #[tokio::test]
    async fn at_most_once_saves_the_batch_end_once_and_goes_past_a_failing_record() {
        let case = Case::new("amo-batch");
        let batch = Batch {
            records: vec![
                to_project(1, true),
                to_project(2, false),
                to_project(3, true),
            ],
            last_observed: 3,
        };

        let (outcome, called_for) = case.apply(DeliveryMode::AtMostOnce, None, batch).await;

        assert_eq!(outcome, BatchOutcome::Settled { through: 3 });
        assert_eq!(
            called_for,
            vec![1, 2, 3],
            "a failing record must not stop the records behind it"
        );
        assert_eq!(case.store.saves(), 1, "the batch is saved once");
        assert_eq!(case.stored().await, Some(3), "at the batch's end");
    }

    // AtLeastOnce: the handlers and the one offset save are both stages that
    // can leave records unprocessed.

    #[tokio::test]
    async fn at_least_once_settles_on_success() {
        let case = Case::new("alo-ok");

        let (outcome, called_for) = case
            .apply(DeliveryMode::AtLeastOnce, None, one_record(true))
            .await;

        assert_eq!(called_for, vec![SEQUENCE], "the handler must be called");
        assert_eq!(
            outcome,
            BatchOutcome::Settled { through: SEQUENCE },
            "the batch must settle through the processed sequence"
        );
        assert_eq!(
            case.stored().await,
            Some(SEQUENCE),
            "AtLeastOnce must persist the new offset"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_on_project_failure() {
        let case = Case::new("alo-project-fail");

        let (outcome, _) = case
            .apply(DeliveryMode::AtLeastOnce, None, one_record(false))
            .await;

        assert_eq!(outcome, BatchOutcome::Unsettled);
        assert_eq!(
            case.store.saves(),
            0,
            "a failed project must not reach the offset store"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_when_the_offset_save_fails() {
        let case = Case::refusing_saves("alo-save-fail");

        let (outcome, _) = case
            .apply(DeliveryMode::AtLeastOnce, None, one_record(true))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "a successful project whose offset did not land is not processed"
        );
        assert_eq!(case.stored().await, None);
    }

    #[tokio::test]
    async fn at_least_once_saves_through_the_record_before_a_failure() {
        let case = Case::new("alo-partial");
        let batch = Batch {
            records: vec![
                to_project(1, true),
                skipped(2),
                to_project(3, false),
                to_project(4, true),
            ],
            last_observed: 4,
        };

        let (outcome, called_for) = case.apply(DeliveryMode::AtLeastOnce, None, batch).await;

        assert_eq!(
            outcome,
            BatchOutcome::Settled { through: 2 },
            "the skip before the failure is settled too, so the batch settles through it"
        );
        assert_eq!(
            called_for,
            vec![1, 3],
            "nothing behind the failing record may be projected"
        );
        assert_eq!(case.store.saves(), 1, "the batch is saved once");
        assert_eq!(case.stored().await, Some(2));
    }

    #[tokio::test]
    async fn at_least_once_does_not_settle_a_partial_batch_whose_save_fails() {
        let case = Case::refusing_saves("alo-partial-save-fail");
        let batch = Batch {
            records: vec![to_project(1, true), to_project(2, false)],
            last_observed: 2,
        };

        let (outcome, _) = case.apply(DeliveryMode::AtLeastOnce, None, batch).await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "the part that succeeded is not settled when its offset did not land"
        );
    }

    #[tokio::test]
    async fn at_least_once_does_not_save_when_only_skips_precede_a_failure() {
        let case = Case::new("alo-skip-then-fail");
        let batch = Batch {
            records: vec![skipped(1), to_project(2, false)],
            last_observed: 2,
        };

        let (outcome, _) = case.apply(DeliveryMode::AtLeastOnce, None, batch).await;

        assert_eq!(outcome, BatchOutcome::Settled { through: 1 });
        assert_eq!(
            case.store.saves(),
            0,
            "a skip alone leaves nothing to make durable"
        );
    }

    #[tokio::test]
    async fn a_batch_that_settles_whole_settles_through_its_last_observed_position() {
        let case = Case::new("alo-dropped-tail");
        // The poller read through 3 but its transform dropped 2 and 3.
        let batch = Batch {
            records: vec![to_project(1, true)],
            last_observed: 3,
        };

        let (outcome, _) = case.apply(DeliveryMode::AtLeastOnce, None, batch).await;

        assert_eq!(outcome, BatchOutcome::Settled { through: 3 });
        assert_eq!(case.stored().await, Some(3));
    }

    // ExactlyOnce without a TxProvider: each handler's own `Result` is the
    // whole account, and the framework saves nothing.

    #[tokio::test]
    async fn exactly_once_without_tx_settles_on_success_without_saving() {
        let case = Case::new("eo-notx-ok");

        let (outcome, _) = case
            .apply(DeliveryMode::ExactlyOnce, None, one_record(true))
            .await;

        assert_eq!(outcome, BatchOutcome::Settled { through: SEQUENCE });
        assert_eq!(
            case.store.saves(),
            0,
            "ExactlyOnce without a TxProvider must NOT save the offset via the framework"
        );
        assert_eq!(case.stored().await, None);
    }

    #[tokio::test]
    async fn exactly_once_without_tx_retries_on_project_failure() {
        let case = Case::new("eo-notx-fail");

        let (outcome, _) = case
            .apply(DeliveryMode::ExactlyOnce, None, one_record(false))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "a self-reported project failure is not a processed event"
        );
        assert_eq!(
            case.store.saves(),
            0,
            "the framework still owns no offset in this mode"
        );
    }

    #[tokio::test]
    async fn exactly_once_without_tx_settles_up_to_the_record_before_a_failure() {
        let case = Case::new("eo-notx-partial");
        let batch = Batch {
            records: vec![
                to_project(1, true),
                to_project(2, false),
                to_project(3, true),
            ],
            last_observed: 3,
        };

        let (outcome, called_for) = case.apply(DeliveryMode::ExactlyOnce, None, batch).await;

        assert_eq!(outcome, BatchOutcome::Settled { through: 1 });
        assert_eq!(called_for, vec![1, 2]);
        assert_eq!(case.store.saves(), 0);
    }

    // ExactlyOnce with a TxProvider: begin, the handlers, the offset save and
    // commit are the stages, and a failure at any of them leaves the whole
    // batch unapplied.

    #[tokio::test]
    async fn exactly_once_with_tx_settles_on_success() {
        let case = Case::new("eo-tx-ok");
        let provider = TestTxProvider::succeeding();

        let (outcome, _) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), one_record(true))
            .await;

        assert_eq!(outcome, BatchOutcome::Settled { through: SEQUENCE });
        assert_eq!(provider.committed(), 1);
        assert_eq!(provider.rolled_back(), 0);
        assert_eq!(case.stored().await, Some(SEQUENCE));
    }

    #[tokio::test]
    async fn exactly_once_with_tx_commits_a_batch_once() {
        let case = Case::new("eo-tx-batch");
        let provider = TestTxProvider::succeeding();
        let batch = Batch {
            records: vec![
                to_project(1, true),
                to_project(2, true),
                to_project(3, true),
            ],
            last_observed: 3,
        };

        let (outcome, called_for) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), batch)
            .await;

        assert_eq!(outcome, BatchOutcome::Settled { through: 3 });
        assert_eq!(called_for, vec![1, 2, 3]);
        assert_eq!(provider.begun(), 1, "one transaction for the batch");
        assert_eq!(provider.committed(), 1, "one commit for the batch");
        assert_eq!(case.store.saves(), 1, "one save for the batch");
        assert_eq!(case.stored().await, Some(3), "at the batch's end");
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_begin_fails() {
        let case = Case::new("eo-tx-begin-fail");
        let provider = TestTxProvider::new(true, false);

        let (outcome, called_for) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), one_record(true))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "a transaction that never started leaves the batch unapplied, not delivered"
        );
        assert!(
            called_for.is_empty(),
            "project must not run without the transaction it was promised"
        );
        assert_eq!(case.store.saves(), 0);
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_project_fails() {
        let case = Case::new("eo-tx-project-fail");
        let provider = TestTxProvider::succeeding();

        let (outcome, _) = case
            .apply(
                DeliveryMode::ExactlyOnce,
                Some(&provider),
                one_record(false),
            )
            .await;

        assert_eq!(outcome, BatchOutcome::Unsettled);
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
        assert_eq!(
            case.store.saves(),
            0,
            "offset store must not be touched when a handler fails"
        );
    }

    #[tokio::test]
    async fn exactly_once_with_tx_stops_the_batch_at_the_first_failure() {
        let case = Case::new("eo-tx-batch-fail");
        let provider = TestTxProvider::succeeding();
        let batch = Batch {
            records: vec![
                to_project(1, true),
                to_project(2, false),
                to_project(3, true),
            ],
            last_observed: 3,
        };

        let (outcome, called_for) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), batch)
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "the records before the failure shared the rolled-back transaction, so \
             none of them is settled"
        );
        assert_eq!(
            called_for,
            vec![1, 2],
            "nothing behind the failing record may be projected"
        );
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
        assert_eq!(case.store.saves(), 0);
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_the_offset_save_fails() {
        let case = Case::refusing_saves("eo-tx-save-fail");
        let provider = TestTxProvider::succeeding();

        let (outcome, _) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), one_record(true))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "a successful project does not settle a batch whose transaction was rolled back"
        );
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
    }

    /// A failed `commit` is indistinguishable from a commit whose answer was
    /// lost, and the transaction is already gone.  Reading it as uncommitted is
    /// what keeps the batch from disappearing.
    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_commit_fails() {
        let case = Case::new("eo-tx-commit-fail");
        let provider = TestTxProvider::new(false, true);

        let (outcome, _) = case
            .apply(DeliveryMode::ExactlyOnce, Some(&provider), one_record(true))
            .await;

        assert_eq!(
            outcome,
            BatchOutcome::Unsettled,
            "an unknown commit outcome must not advance the offset"
        );
        assert_eq!(
            provider.rolled_back(),
            0,
            "commit consumed the transaction; there is nothing left to roll back"
        );
    }

    // A batch of skips owes nothing, whatever the mode.

    #[tokio::test]
    async fn a_batch_of_only_skips_settles_whole_without_any_io() {
        let cases = [
            (DeliveryMode::AtMostOnce, false),
            (DeliveryMode::AtLeastOnce, false),
            (DeliveryMode::ExactlyOnce, false),
            (DeliveryMode::ExactlyOnce, true),
        ];
        for (mode, with_tx) in cases {
            let case = Case::new("all-skipped");
            let provider = TestTxProvider::succeeding();
            let batch = Batch {
                records: vec![skipped(1), skipped(2)],
                last_observed: 2,
            };

            let (outcome, called_for) = case.apply(mode, with_tx.then_some(&provider), batch).await;

            assert_eq!(
                outcome,
                BatchOutcome::Settled { through: 2 },
                "{mode:?} (tx: {with_tx})"
            );
            assert!(called_for.is_empty(), "{mode:?} (tx: {with_tx})");
            assert_eq!(case.store.saves(), 0, "{mode:?} (tx: {with_tx}) saved");
            assert_eq!(provider.begun(), 0, "{mode:?} (tx: {with_tx}) began");
        }
    }
}
