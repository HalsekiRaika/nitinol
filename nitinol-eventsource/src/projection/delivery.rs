use std::future::Future;
use std::pin::Pin;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::ProjectionId;

use crate::projection::context::ProjectionContext;

/// Single entry point that runs the full per-event orchestration:
/// `pre_project` → `ProjectionContext::new` → `project_fn` → `post_project`.
///
/// The two code paths (catch-up loop and live recv) differ only in how they
/// invoke `project_fn`; all surrounding bookkeeping is identical and lives here.
pub(crate) async fn apply<'pid, Cs, F>(
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
            &'ctx mut ProjectionContext<'pid, ()>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
                    + Send
                    + 'ctx,
            >,
        > + Send,
{
    pre_project(mode, store, pid, sequence).await;
    let mut ctx = ProjectionContext::new(pid, sequence, mode, None::<()>);
    let result = project_fn(&mut ctx).await;
    post_project(mode, store, pid, current_checkpoint, sequence, result).await
}

/// Save the checkpoint before calling `project()` for AtMostOnce delivery.
///
/// For other modes this is a no-op; the checkpoint is handled in
/// `post_project` instead.
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

/// Apply post-project checkpoint logic and return the new `checkpoint_sequence`.
///
/// | Mode         | Behaviour on project success | Behaviour on project failure |
/// |--------------|------------------------------|------------------------------|
/// | AtMostOnce   | checkpoint already saved; no-op | log warn; checkpoint already saved |
/// | AtLeastOnce  | save checkpoint; advance       | log warn; do NOT advance    |
/// | ExactlyOnce  | advance in-memory dedup counter (user saved checkpoint inside project) | log warn; still advance dedup counter |
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
            // Checkpoint was saved before project(); just log any error.
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
                        // Do not advance — event will be reprocessed on restart.
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
            // The framework never saves the checkpoint for ExactlyOnce.
            // The user is responsible for saving both the read-model update
            // and the checkpoint atomically inside project().
            if let Err(e) = result {
                tracing::warn!(
                    error = %e,
                    sequence,
                    "project() failed (exactly-once)",
                );
            }
            // Advance the in-memory dedup counter so live events are deduplicated.
            sequence
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests — re-prevention for dry-violation (ARCH-NEW-projection-process-orchestration-dry)
//
// These tests exercise `apply` directly so that any future divergence of the
// catch-up and live code paths (re-inlining the orchestration) is immediately
// visible as a gap between `apply`-level coverage and path-level coverage.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use nitinol_persistence::store::InMemoryCheckpointStore;
    use nitinol_persistence::ProjectionId;

    /// `apply` with AtLeastOnce: project_fn is called and the new checkpoint is
    /// returned when it succeeds.
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
            /*current_checkpoint=*/ 0,
            /*sequence=*/ 1,
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

    /// `apply` with AtMostOnce: checkpoint is saved BEFORE project_fn is called.
    /// On project_fn failure the checkpoint is still advanced (at-most-once semantics).
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
            move |_ctx| {
                Box::pin(async move {
                    Err(Box::<dyn std::error::Error + Send + Sync>::from("fail"))
                })
            },
        )
        .await;

        // AtMostOnce: checkpoint advances even when project fails
        assert_eq!(new_seq, 2);
        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            Some(2),
            "AtMostOnce must have saved the checkpoint before the (failed) project"
        );
    }

    /// `apply` with ExactlyOnce: framework does NOT persist the checkpoint.
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
            |_ctx| Box::pin(async move { Ok(()) }),
        )
        .await;

        assert_eq!(
            store.load(&pid).await.expect("load ok"),
            None,
            "ExactlyOnce must NOT save the checkpoint via the framework"
        );
    }
}
