use std::future::Future;
use std::pin::Pin;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::ProjectionId;

use crate::projection::context::ProjectionContext;
use crate::projection::tx_provider::ErasedTxProvider;

/// What one application attempt means for the subscription cursor.
///
/// The decision cannot be recovered from the checkpoint value the attempt
/// produced: an event whose `current_checkpoint` already equals its own
/// sequence yields the same number whether it was applied or held.  Only
/// [`apply`] knows which delivery mode ran and which stage of it failed, so it
/// states the classification and the new checkpoint as one value, and its
/// caller projects both from that one value rather than re-deciding either.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    /// Nothing further is owed for this event — it was applied, or the delivery
    /// mode in force declares a failed application delivered.
    Settled { checkpoint: u64 },
    /// The event was not applied.  Holding the cursor here is what gets it
    /// redelivered instead of skipped.
    Retry { checkpoint: u64 },
}

impl ApplyOutcome {
    /// The in-memory checkpoint the projector carries into the next event.
    pub(crate) fn checkpoint(&self) -> u64 {
        match self {
            Self::Settled { checkpoint } | Self::Retry { checkpoint } => *checkpoint,
        }
    }
}

//noinspection DuplicatedCode
pub(crate) async fn apply<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    tx_provider: Option<&(dyn ErasedTxProvider<Cs::Tx> + Send + Sync)>,
    project_fn: F,
) -> ApplyOutcome
where
    Cs: CheckpointStore,
    Cs::Tx: 'static,
    F: for<'ctx> FnOnce(
            &'ctx mut ProjectionContext<'pid, Cs::Tx>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
                    + Send
                    + 'ctx,
            >,
        > + Send,
{
    match tx_provider {
        Some(provider) if mode == DeliveryMode::ExactlyOnce => {
            apply_with_tx(
                mode,
                store,
                pid,
                current_checkpoint,
                sequence,
                provider,
                project_fn,
            )
            .await
        }
        _ => apply_no_tx(mode, store, pid, current_checkpoint, sequence, project_fn).await,
    }
}

//noinspection DuplicatedCode
async fn apply_with_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    provider: &(dyn ErasedTxProvider<Cs::Tx> + Send + Sync),
    project_fn: F,
) -> ApplyOutcome
where
    Cs: CheckpointStore,
    Cs::Tx: 'static,
    F: for<'ctx> FnOnce(
            &'ctx mut ProjectionContext<'pid, Cs::Tx>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
                    + Send
                    + 'ctx,
            >,
        > + Send,
{
    let tx = match ErasedTxProvider::begin(provider).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                error = %e,
                sequence,
                "TxProvider::begin failed; event not applied and will be redelivered",
            );
            return ApplyOutcome::Retry {
                checkpoint: current_checkpoint,
            };
        }
    };

    let mut ctx = ProjectionContext::new(pid, sequence, mode, Some(tx));
    let result = project_fn(&mut ctx).await;
    let mut tx_opt = ctx.take_tx();

    match result {
        Ok(()) => {
            if let Some(ref mut tx) = tx_opt {
                if let Err(e) = store.save(pid, sequence, Some(tx)).await {
                    tracing::error!(
                        error = %e,
                        sequence,
                        "checkpoint save failed (exactly-once + tx); rolling back",
                    );
                    if let Some(tx) = tx_opt {
                        ErasedTxProvider::rollback(provider, tx).await;
                    }
                    return ApplyOutcome::Retry {
                        checkpoint: current_checkpoint,
                    };
                }
            }
            if let Some(tx) = tx_opt {
                if let Err(e) = ErasedTxProvider::commit(provider, tx).await {
                    // `commit` consumed the transaction and answers with a
                    // single `Err`, so a lost reply from a backend that did
                    // commit is indistinguishable here from one that did not.
                    // Reading it as uncommitted risks a duplicate application;
                    // reading it as committed loses the event outright.
                    tracing::error!(
                        error = %e,
                        sequence,
                        "TxProvider::commit failed; treating the event as uncommitted \
                         and redelivering it",
                    );
                    return ApplyOutcome::Retry {
                        checkpoint: current_checkpoint,
                    };
                }
            }
            ApplyOutcome::Settled {
                checkpoint: sequence,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, sequence, "project() failed (exactly-once + tx); rolling back");
            if let Some(tx) = tx_opt {
                ErasedTxProvider::rollback(provider, tx).await;
            }
            ApplyOutcome::Retry {
                checkpoint: current_checkpoint,
            }
        }
    }
}

//noinspection DuplicatedCode
async fn apply_no_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    project_fn: F,
) -> ApplyOutcome
where
    Cs: CheckpointStore,
    F: for<'ctx> FnOnce(
            &'ctx mut ProjectionContext<'pid, Cs::Tx>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
                    + Send
                    + 'ctx,
            >,
        > + Send,
{
    pre_project(mode, store, pid, sequence).await;
    let mut ctx = ProjectionContext::new(pid, sequence, mode, None);
    let result = project_fn(&mut ctx).await;
    post_project(mode, store, pid, current_checkpoint, sequence, result).await
}

pub(crate) async fn pre_project<Cs: CheckpointStore>(
    mode: DeliveryMode,
    checkpoint_store: &Cs,
    projection_id: &ProjectionId,
    sequence: u64,
) {
    if mode == DeliveryMode::AtMostOnce {
        if let Err(e) = checkpoint_store.save(projection_id, sequence, None).await {
            tracing::error!(
                error = %e,
                sequence,
                "checkpoint save failed before project (at-most-once)",
            );
        }
    }
}

pub(crate) async fn post_project<Cs: CheckpointStore>(
    mode: DeliveryMode,
    checkpoint_store: &Cs,
    projection_id: &ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    result: Result<(), Box<dyn std::error::Error + Send + Sync>>,
) -> ApplyOutcome {
    match mode {
        // At-most-once persisted the checkpoint before the handler ran.  The
        // event is spent either way — that is the guarantee the mode names.
        DeliveryMode::AtMostOnce => {
            if let Err(e) = result {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (at-most-once); event skipped",
                );
            }
            ApplyOutcome::Settled {
                checkpoint: sequence,
            }
        }
        DeliveryMode::AtLeastOnce => match result {
            Ok(()) => {
                if let Err(e) = checkpoint_store.save(projection_id, sequence, None).await {
                    tracing::error!(
                        error = %e,
                        sequence,
                        "checkpoint save failed after project (at-least-once); \
                         event will be redelivered",
                    );
                    ApplyOutcome::Retry {
                        checkpoint: current_checkpoint,
                    }
                } else {
                    ApplyOutcome::Settled {
                        checkpoint: sequence,
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed; checkpoint not advanced (at-least-once)",
                );
                ApplyOutcome::Retry {
                    checkpoint: current_checkpoint,
                }
            }
        },
        // Without a TxProvider the framework saves no checkpoint, so the
        // `Result` of `project` is the only account it has of what happened.
        // Taking a self-reported failure as processed is the one reading that
        // contradicts the mode's own guarantee.
        DeliveryMode::ExactlyOnce => match result {
            Ok(()) => ApplyOutcome::Settled {
                checkpoint: sequence,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (exactly-once without a TxProvider); \
                     event will be redelivered",
                );
                ApplyOutcome::Retry {
                    checkpoint: current_checkpoint,
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use nitinol_persistence::error::CheckpointError;
    use nitinol_persistence::store::{CheckpointScope, InMemoryCheckpointStore};
    use nitinol_persistence::ProjectionId;

    /// The checkpoint every case starts from.  Deliberately non-zero and
    /// different from the sequence under test, so a `Retry` that wrongly
    /// reported the event's own sequence is visible in the value as well as in
    /// the variant.
    const CURRENT: u64 = 4;
    const SEQUENCE: u64 = 9;

    fn failure() -> Box<dyn std::error::Error + Send + Sync> {
        Box::<dyn std::error::Error + Send + Sync>::from("intentional failure")
    }

    /// A `CheckpointStore` that can be told to refuse every save, so the
    /// "handler succeeded but the checkpoint did not land" stage is reachable.
    struct TestCheckpointStore {
        inner: InMemoryCheckpointStore,
        fail_save: bool,
        saves: AtomicU64,
    }

    impl TestCheckpointStore {
        fn new(fail_save: bool) -> Self {
            Self {
                inner: InMemoryCheckpointStore::default(),
                fail_save,
                saves: AtomicU64::new(0),
            }
        }

        fn saves(&self) -> u64 {
            self.saves.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CheckpointStore for TestCheckpointStore {
        type Tx = ();

        async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError> {
            self.inner.load(scope).await
        }

        async fn save(
            &self,
            scope: &dyn CheckpointScope,
            sequence: u64,
            tx: Option<&mut Self::Tx>,
        ) -> Result<(), CheckpointError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            if self.fail_save {
                return Err(CheckpointError::Backend(failure()));
            }
            self.inner.save(scope, sequence, tx).await
        }
    }

    /// A transaction provider that can be told to fail `begin` or `commit`, and
    /// that counts what the delivery engine asked of it.
    struct TestTxProvider {
        fail_begin: bool,
        fail_commit: bool,
        committed: AtomicU64,
        rolled_back: AtomicU64,
    }

    impl TestTxProvider {
        fn new(fail_begin: bool, fail_commit: bool) -> Self {
            Self {
                fail_begin,
                fail_commit,
                committed: AtomicU64::new(0),
                rolled_back: AtomicU64::new(0),
            }
        }

        fn succeeding() -> Self {
            Self::new(false, false)
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

    fn erased(provider: &TestTxProvider) -> Option<&(dyn ErasedTxProvider<()> + Send + Sync)> {
        Some(provider as &(dyn ErasedTxProvider<()> + Send + Sync))
    }

    // AtMostOnce: the checkpoint is spent before the handler runs, so both
    // outcomes settle.

    #[tokio::test]
    async fn at_most_once_settles_on_success() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("amo-ok");

        let outcome = apply(
            DeliveryMode::AtMostOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Settled {
                checkpoint: SEQUENCE
            }
        );
        assert_eq!(store.load(&pid).await.expect("load ok"), Some(SEQUENCE));
    }

    #[tokio::test]
    async fn at_most_once_settles_on_project_failure_after_saving_the_checkpoint() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("amo-fail");

        let outcome = apply(
            DeliveryMode::AtMostOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Settled {
                checkpoint: SEQUENCE
            },
            "AtMostOnce trades a failed handler away; it must not ask for a retry"
        );
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            Some(SEQUENCE),
            "AtMostOnce must have saved the checkpoint before the (failed) project"
        );
    }

    // AtLeastOnce: the handler and the checkpoint save are both stages that can
    // leave the event unprocessed.

    #[tokio::test]
    async fn at_least_once_settles_on_success() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("alo-ok");
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_c = std::sync::Arc::clone(&called);

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            move |_ctx| {
                called_c.store(true, Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            },
        )
        .await;

        assert!(called.load(Ordering::SeqCst), "project_fn must be called");
        assert_eq!(
            outcome,
            ApplyOutcome::Settled {
                checkpoint: SEQUENCE
            },
            "checkpoint must advance to the processed sequence"
        );
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            Some(SEQUENCE),
            "AtLeastOnce must persist the new checkpoint"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_on_project_failure() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("alo-project-fail");

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            }
        );
        assert_eq!(
            store.saves(),
            0,
            "a failed project must not reach the checkpoint store"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_when_the_checkpoint_save_fails() {
        let store = TestCheckpointStore::new(true);
        let pid = ProjectionId::new("alo-save-fail");

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            },
            "a successful project whose checkpoint did not land is not processed"
        );
        assert_eq!(store.load(&pid).await.expect("load ok"), None);
    }

    // ExactlyOnce without a TxProvider: `project`'s own `Result` is the whole
    // account, and the framework saves nothing.

    #[tokio::test]
    async fn exactly_once_without_tx_settles_on_success_without_saving() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-notx-ok");

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Settled {
                checkpoint: SEQUENCE
            }
        );
        assert_eq!(
            store.saves(),
            0,
            "ExactlyOnce without a TxProvider must NOT save the checkpoint via the framework"
        );
        assert_eq!(store.load(&pid).await.expect("load ok"), None);
    }

    #[tokio::test]
    async fn exactly_once_without_tx_retries_on_project_failure() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-notx-fail");

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            },
            "a self-reported project failure is not a processed event"
        );
        assert_eq!(
            store.saves(),
            0,
            "the framework still owns no checkpoint in this mode"
        );
    }

    // ExactlyOnce with a TxProvider: begin, project, checkpoint save and commit
    // are four stages, and a failure at any of them leaves the event unapplied.

    #[tokio::test]
    async fn exactly_once_with_tx_settles_on_success() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-tx-ok");
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Settled {
                checkpoint: SEQUENCE
            }
        );
        assert_eq!(provider.committed(), 1);
        assert_eq!(provider.rolled_back(), 0);
        assert_eq!(store.load(&pid).await.expect("load ok"), Some(SEQUENCE));
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_begin_fails() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-tx-begin-fail");
        let provider = TestTxProvider::new(true, false);
        let projected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let projected_c = std::sync::Arc::clone(&projected);

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            move |_ctx| {
                projected_c.store(true, Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            },
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            },
            "a transaction that never started leaves the event unapplied, not delivered"
        );
        assert!(
            !projected.load(Ordering::SeqCst),
            "project must not run without the transaction it was promised"
        );
        assert_eq!(store.saves(), 0);
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_project_fails() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-tx-project-fail");
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            }
        );
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
        assert_eq!(
            store.saves(),
            0,
            "checkpoint store must not be touched when project_fn fails"
        );
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_the_checkpoint_save_fails() {
        let store = TestCheckpointStore::new(true);
        let pid = ProjectionId::new("eo-tx-save-fail");
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            },
            "a successful project does not settle an event whose transaction was rolled back"
        );
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
    }

    /// A failed `commit` is indistinguishable from a commit whose answer was
    /// lost, and the transaction is already gone.  Reading it as uncommitted is
    /// what keeps the event from disappearing.
    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_commit_fails() {
        let store = TestCheckpointStore::new(false);
        let pid = ProjectionId::new("eo-tx-commit-fail");
        let provider = TestTxProvider::new(false, true);

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry {
                checkpoint: CURRENT
            },
            "an unknown commit outcome must not advance the checkpoint"
        );
        assert_eq!(
            provider.rolled_back(),
            0,
            "commit consumed the transaction; there is nothing left to roll back"
        );
    }
}
