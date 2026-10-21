use std::future::Future;
use std::pin::Pin;

use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};

use crate::projection::ProjectionId;

use crate::projection::context::ProjectionContext;
use crate::projection::tx_provider::ErasedTxProvider;

/// What one application attempt means for the subscription cursor.
///
/// The decision cannot be recovered from the offset value the attempt
/// produced: an event whose `current_offset` already equals its own
/// sequence yields the same number whether it was applied or held.  Only
/// [`apply`] knows which delivery mode ran and which stage of it failed, so it
/// states the classification and the new offset as one value, and its
/// caller projects both from that one value rather than re-deciding either.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    /// Nothing further is owed for this event — it was applied, or the delivery
    /// mode in force declares a failed application delivered.
    Settled { offset: u64 },
    /// The event was not applied.  Holding the cursor here is what gets it
    /// redelivered instead of skipped.
    Retry { offset: u64 },
}

impl ApplyOutcome {
    /// The in-memory offset the projector carries into the next event.
    pub(crate) fn offset(&self) -> u64 {
        match self {
            Self::Settled { offset } | Self::Retry { offset } => *offset,
        }
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

//noinspection DuplicatedCode
pub(crate) async fn apply<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    current_offset: u64,
    sequence: u64,
    tx_provider: Option<&(dyn ErasedTxProvider<Cs::Tx> + Send + Sync)>,
    project_fn: F,
) -> ApplyOutcome
where
    Cs: OffsetStore,
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
                projection,
                current_offset,
                sequence,
                provider,
                project_fn,
            )
            .await
        }
        _ => {
            apply_no_tx(
                mode,
                store,
                projection,
                current_offset,
                sequence,
                project_fn,
            )
            .await
        }
    }
}

//noinspection DuplicatedCode
async fn apply_with_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    current_offset: u64,
    sequence: u64,
    provider: &(dyn ErasedTxProvider<Cs::Tx> + Send + Sync),
    project_fn: F,
) -> ApplyOutcome
where
    Cs: OffsetStore,
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
                offset: current_offset,
            };
        }
    };

    let mut ctx = ProjectionContext::new(projection.id, sequence, mode, Some(tx));
    let result = project_fn(&mut ctx).await;
    let mut tx_opt = ctx.take_tx();

    match result {
        Ok(()) => {
            if let Some(ref mut tx) = tx_opt {
                if let Err(e) = store.save(projection.scope, sequence, Some(tx)).await {
                    tracing::error!(
                        error = %e,
                        sequence,
                        "offset save failed (exactly-once + tx); rolling back",
                    );
                    if let Some(tx) = tx_opt {
                        ErasedTxProvider::rollback(provider, tx).await;
                    }
                    return ApplyOutcome::Retry {
                        offset: current_offset,
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
                        offset: current_offset,
                    };
                }
            }
            ApplyOutcome::Settled { offset: sequence }
        }
        Err(e) => {
            tracing::warn!(error = %e, sequence, "project() failed (exactly-once + tx); rolling back");
            if let Some(tx) = tx_opt {
                ErasedTxProvider::rollback(provider, tx).await;
            }
            ApplyOutcome::Retry {
                offset: current_offset,
            }
        }
    }
}

//noinspection DuplicatedCode
async fn apply_no_tx<'pid, Cs, F>(
    mode: DeliveryMode,
    store: &Cs,
    projection: ProjectionIdentity<'pid>,
    current_offset: u64,
    sequence: u64,
    project_fn: F,
) -> ApplyOutcome
where
    Cs: OffsetStore,
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
    pre_project(mode, store, projection.scope, sequence).await;
    let mut ctx = ProjectionContext::new(projection.id, sequence, mode, None);
    let result = project_fn(&mut ctx).await;
    post_project(
        mode,
        store,
        projection.scope,
        current_offset,
        sequence,
        result,
    )
    .await
}

pub(crate) async fn pre_project<Cs: OffsetStore>(
    mode: DeliveryMode,
    offset_store: &Cs,
    scope: &OffsetScope,
    sequence: u64,
) {
    if mode == DeliveryMode::AtMostOnce {
        if let Err(e) = offset_store.save(scope, sequence, None).await {
            tracing::error!(
                error = %e,
                sequence,
                "offset save failed before project (at-most-once)",
            );
        }
    }
}

pub(crate) async fn post_project<Cs: OffsetStore>(
    mode: DeliveryMode,
    offset_store: &Cs,
    scope: &OffsetScope,
    current_offset: u64,
    sequence: u64,
    result: Result<(), Box<dyn std::error::Error + Send + Sync>>,
) -> ApplyOutcome {
    match mode {
        // At-most-once persisted the offset before the handler ran.  The
        // event is spent either way — that is the guarantee the mode names.
        DeliveryMode::AtMostOnce => {
            if let Err(e) = result {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (at-most-once); event skipped",
                );
            }
            ApplyOutcome::Settled { offset: sequence }
        }
        DeliveryMode::AtLeastOnce => match result {
            Ok(()) => {
                if let Err(e) = offset_store.save(scope, sequence, None).await {
                    tracing::error!(
                        error = %e,
                        sequence,
                        "offset save failed after project (at-least-once); \
                         event will be redelivered",
                    );
                    ApplyOutcome::Retry {
                        offset: current_offset,
                    }
                } else {
                    ApplyOutcome::Settled { offset: sequence }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed; offset not advanced (at-least-once)",
                );
                ApplyOutcome::Retry {
                    offset: current_offset,
                }
            }
        },
        // Without a TxProvider the framework saves no offset, so the
        // `Result` of `project` is the only account it has of what happened.
        // Taking a self-reported failure as processed is the one reading that
        // contradicts the mode's own guarantee.
        DeliveryMode::ExactlyOnce => match result {
            Ok(()) => ApplyOutcome::Settled { offset: sequence },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (exactly-once without a TxProvider); \
                     event will be redelivered",
                );
                ApplyOutcome::Retry {
                    offset: current_offset,
                }
            }
        },
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

    /// The offset every case starts from.  Deliberately non-zero and
    /// different from the sequence under test, so a `Retry` that wrongly
    /// reported the event's own sequence is visible in the value as well as in
    /// the variant.
    const CURRENT: u64 = 4;
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

    // AtMostOnce: the offset is spent before the handler runs, so both
    // outcomes settle.

    #[tokio::test]
    async fn at_most_once_settles_on_success() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("amo-ok");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::AtMostOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(outcome, ApplyOutcome::Settled { offset: SEQUENCE });
        assert_eq!(store.load(&scope).await.expect("load ok"), Some(SEQUENCE));
    }

    #[tokio::test]
    async fn at_most_once_settles_on_project_failure_after_saving_the_offset() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("amo-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::AtMostOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Settled { offset: SEQUENCE },
            "AtMostOnce trades a failed handler away; it must not ask for a retry"
        );
        assert_eq!(
            store.load(&scope).await.expect("load ok"),
            Some(SEQUENCE),
            "AtMostOnce must have saved the offset before the (failed) project"
        );
    }

    // AtLeastOnce: the handler and the offset save are both stages that can
    // leave the event unprocessed.

    #[tokio::test]
    async fn at_least_once_settles_on_success() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("alo-ok");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_c = std::sync::Arc::clone(&called);

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            identity,
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
            ApplyOutcome::Settled { offset: SEQUENCE },
            "offset must advance to the processed sequence"
        );
        assert_eq!(
            store.load(&scope).await.expect("load ok"),
            Some(SEQUENCE),
            "AtLeastOnce must persist the new offset"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_on_project_failure() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("alo-project-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(outcome, ApplyOutcome::Retry { offset: CURRENT });
        assert_eq!(
            store.saves(),
            0,
            "a failed project must not reach the offset store"
        );
    }

    #[tokio::test]
    async fn at_least_once_retries_when_the_offset_save_fails() {
        let store = TestOffsetStore::new(true);
        let pid = ProjectionId::new("alo-save-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::AtLeastOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry { offset: CURRENT },
            "a successful project whose offset did not land is not processed"
        );
        assert_eq!(store.load(&scope).await.expect("load ok"), None);
    }

    // ExactlyOnce without a TxProvider: `project`'s own `Result` is the whole
    // account, and the framework saves nothing.

    #[tokio::test]
    async fn exactly_once_without_tx_settles_on_success_without_saving() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-notx-ok");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(outcome, ApplyOutcome::Settled { offset: SEQUENCE });
        assert_eq!(
            store.saves(),
            0,
            "ExactlyOnce without a TxProvider must NOT save the offset via the framework"
        );
        assert_eq!(store.load(&scope).await.expect("load ok"), None);
    }

    #[tokio::test]
    async fn exactly_once_without_tx_retries_on_project_failure() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-notx-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            None,
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry { offset: CURRENT },
            "a self-reported project failure is not a processed event"
        );
        assert_eq!(
            store.saves(),
            0,
            "the framework still owns no offset in this mode"
        );
    }

    // ExactlyOnce with a TxProvider: begin, project, offset save and commit
    // are four stages, and a failure at any of them leaves the event unapplied.

    #[tokio::test]
    async fn exactly_once_with_tx_settles_on_success() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-tx-ok");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(outcome, ApplyOutcome::Settled { offset: SEQUENCE });
        assert_eq!(provider.committed(), 1);
        assert_eq!(provider.rolled_back(), 0);
        assert_eq!(store.load(&scope).await.expect("load ok"), Some(SEQUENCE));
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_begin_fails() {
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-tx-begin-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let provider = TestTxProvider::new(true, false);
        let projected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let projected_c = std::sync::Arc::clone(&projected);

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
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
            ApplyOutcome::Retry { offset: CURRENT },
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
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-tx-project-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Err(failure()) }),
        )
        .await;

        assert_eq!(outcome, ApplyOutcome::Retry { offset: CURRENT });
        assert_eq!(provider.rolled_back(), 1);
        assert_eq!(provider.committed(), 0);
        assert_eq!(
            store.saves(),
            0,
            "offset store must not be touched when project_fn fails"
        );
    }

    #[tokio::test]
    async fn exactly_once_with_tx_retries_when_the_offset_save_fails() {
        let store = TestOffsetStore::new(true);
        let pid = ProjectionId::new("eo-tx-save-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let provider = TestTxProvider::succeeding();

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry { offset: CURRENT },
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
        let store = TestOffsetStore::new(false);
        let pid = ProjectionId::new("eo-tx-commit-fail");
        let scope = OffsetScope::from(&pid);
        let identity = ProjectionIdentity {
            id: &pid,
            scope: &scope,
        };
        let provider = TestTxProvider::new(false, true);

        let outcome = apply(
            DeliveryMode::ExactlyOnce,
            &store,
            identity,
            CURRENT,
            SEQUENCE,
            erased(&provider),
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Retry { offset: CURRENT },
            "an unknown commit outcome must not advance the offset"
        );
        assert_eq!(
            provider.rolled_back(),
            0,
            "commit consumed the transaction; there is nothing left to roll back"
        );
    }
}
