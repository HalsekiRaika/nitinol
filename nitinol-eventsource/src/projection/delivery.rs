use std::future::Future;
use std::pin::Pin;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::ProjectionId;

use crate::projection::context::ProjectionContext;
use crate::projection::tx_provider::ErasedTxProvider;

pub(crate) async fn apply<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    tx_provider: Option<&(dyn ErasedTxProvider<Cs::Tx> + Send + Sync)>,
    project_fn: F,
) -> u64
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
    let use_tx = tx_provider.is_some() && mode == DeliveryMode::ExactlyOnce;

    if use_tx {
        let provider = tx_provider.unwrap();
        apply_with_tx(mode, store, pid, current_checkpoint, sequence, provider, project_fn).await
    } else {
        apply_no_tx(mode, store, pid, current_checkpoint, sequence, project_fn).await
    }
}

async fn apply_with_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    provider: &(dyn ErasedTxProvider<Cs::Tx> + Send + Sync),
    project_fn: F,
) -> u64
where
    Cs: CheckpointStore,
    Cs::Tx: 'static,
    F: for<'ctx> FnOnce(
            &'ctx mut ProjectionContext<'pid, Cs::Tx>,
        ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'ctx>>
        + Send,
{
    let tx = match ErasedTxProvider::begin(provider).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, sequence, "TxProvider::begin failed; skipping event");
            return current_checkpoint;
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
                    return current_checkpoint;
                }
            }
            if let Some(tx) = tx_opt {
                if let Err(e) = ErasedTxProvider::commit(provider, tx).await {
                    tracing::error!(
                        error = %e,
                        sequence,
                        "TxProvider::commit failed; checkpoint not advanced",
                    );
                    return current_checkpoint;
                }
            }
            sequence
        }
        Err(e) => {
            tracing::warn!(error = %e, sequence, "project() failed (exactly-once + tx); rolling back");
            if let Some(tx) = tx_opt {
                ErasedTxProvider::rollback(provider, tx).await;
            }
            current_checkpoint
        }
    }
}

async fn apply_no_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    pid: &'pid ProjectionId,
    current_checkpoint: u64,
    sequence: u64,
    project_fn: F,
) -> u64
where
    Cs: CheckpointStore,
    F: for<'ctx> FnOnce(
            &'ctx mut ProjectionContext<'pid, Cs::Tx>,
        ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'ctx>>
        + Send,
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
) -> u64 {
    match mode {
        DeliveryMode::AtMostOnce => {
            if let Err(e) = result {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (at-most-once); event skipped",
                );
            }
            sequence
        }
        DeliveryMode::AtLeastOnce => {
            match result {
                Ok(()) => {
                    if let Err(e) = checkpoint_store.save(projection_id, sequence, None).await {
                        tracing::error!(
                            error = %e,
                            sequence,
                            "checkpoint save failed after project (at-least-once); will retry on restart",
                        );
                        current_checkpoint
                    } else {
                        sequence
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        sequence,
                        "project() failed; checkpoint not advanced (at-least-once)",
                    );
                    current_checkpoint
                }
            }
        }
        DeliveryMode::ExactlyOnce => {
            if let Err(e) = result {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (exactly-once)",
                );
            }
            sequence
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nitinol_persistence::store::InMemoryCheckpointStore;
    use nitinol_persistence::ProjectionId;

    #[tokio::test]
    async fn apply_at_least_once_advances_checkpoint_on_success() {
        let store = InMemoryCheckpointStore::default();
        let pid = ProjectionId::new("apply-test");
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_c = std::sync::Arc::clone(&called);

        let new_seq = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            &pid,
            0,
            1,
            None,
            move |_ctx| {
                called_c.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            },
        )
        .await;

        assert!(called.load(std::sync::atomic::Ordering::SeqCst), "project_fn must be called");
        assert_eq!(new_seq, 1, "checkpoint must advance to the processed sequence");
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            Some(1),
            "AtLeastOnce must persist the new checkpoint"
        );
    }

    #[tokio::test]
    async fn apply_at_most_once_saves_checkpoint_before_project() {
        let store = InMemoryCheckpointStore::default();
        let pid = ProjectionId::new("apply-amo-test");

        let new_seq = apply(
            DeliveryMode::AtMostOnce,
            &store,
            &pid,
            0,
            2,
            None,
            move |_ctx| {
                Box::pin(async move {
                    Err(Box::<dyn std::error::Error + Send + Sync>::from("fail"))
                })
            },
        )
        .await;

        assert_eq!(new_seq, 2);
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            Some(2),
            "AtMostOnce must have saved the checkpoint before the (failed) project"
        );
    }

    #[tokio::test]
    async fn apply_exactly_once_does_not_save_checkpoint() {
        let store = InMemoryCheckpointStore::default();
        let pid = ProjectionId::new("apply-eo-test");

        apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            0,
            3,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            None,
            "ExactlyOnce without TxProvider must NOT save the checkpoint via the framework"
        );
    }

    /// Regression: when `TxProvider::commit` fails, the in-memory checkpoint
    /// counter must NOT advance.
    ///
    /// Before the fix, `apply_with_tx` returned `sequence` even after a failed
    /// commit, silently treating an uncommitted event as processed.
    ///
    /// Note: whether the checkpoint store's durable state is rolled back depends
    /// on the transactional semantics of the underlying `CheckpointStore`
    /// implementation.  `InMemoryCheckpointStore` does not emulate real DB
    /// transactions, so we only verify the in-memory return value here.
    #[tokio::test]
    async fn apply_with_tx_commit_failure_does_not_advance_checkpoint() {
        use async_trait::async_trait;

        // A TxProvider whose begin() succeeds but commit() always fails.
        struct FailingCommitProvider;

        #[async_trait]
        impl ErasedTxProvider<()> for FailingCommitProvider {
            async fn begin(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Ok(())
            }

            async fn commit(
                &self,
                _tx: (),
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "simulated commit failure",
                )))
            }

            async fn rollback(&self, _tx: ()) {}
        }

        let store = InMemoryCheckpointStore::default();
        let pid = ProjectionId::new("commit-fail-test");
        let provider = FailingCommitProvider;

        let new_seq = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            0,  // current_checkpoint
            5,  // sequence being processed
            Some(&provider as &(dyn ErasedTxProvider<()> + Send + Sync)),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            new_seq, 0,
            "in-memory checkpoint must not advance when commit fails"
        );
    }

    /// Regression: when `project_fn` returns `Err` in ExactlyOnce + TxProvider mode,
    /// the in-memory checkpoint counter must NOT advance (rollback, not commit).
    ///
    /// Before the fix, `apply_with_tx` returned `sequence` on project failure,
    /// silently treating a failed event as processed.
    #[tokio::test]
    async fn apply_with_tx_project_failure_does_not_advance_checkpoint() {
        use async_trait::async_trait;

        struct SucceedingProvider;

        #[async_trait]
        impl ErasedTxProvider<()> for SucceedingProvider {
            async fn begin(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Ok(())
            }

            async fn commit(
                &self,
                _tx: (),
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Ok(())
            }

            async fn rollback(&self, _tx: ()) {}
        }

        let store = InMemoryCheckpointStore::default();
        let pid = ProjectionId::new("project-fail-test");
        let provider = SucceedingProvider;

        let new_seq = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            &pid,
            0, // current_checkpoint
            7, // sequence being processed
            Some(&provider as &(dyn ErasedTxProvider<()> + Send + Sync)),
            |_ctx| {
                Box::pin(async move {
                    Err(Box::<dyn std::error::Error + Send + Sync>::from(
                        "intentional project failure",
                    ))
                })
            },
        )
        .await;

        assert_eq!(
            new_seq, 0,
            "in-memory checkpoint must not advance when project_fn fails"
        );
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            None,
            "checkpoint store must not be updated when project_fn fails"
        );
    }
}
