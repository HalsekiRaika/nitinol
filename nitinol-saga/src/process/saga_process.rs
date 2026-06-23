use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableSubscription, EventEnvelope, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::context::SagaContext;
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy, TerminalKind};
use crate::process::interpreter::{run_saga_effect, InterpreterCtx};
use crate::process::replay::replay_and_redispatch;
use crate::saga::Saga;

/// Factory invoked during crash-restart replay to reconstruct a
/// [`TellIntent`] from the crash-restart bytes stored in the `TellRequested`
/// outbox marker.
///
/// The closure receives the bytes that were supplied via
/// [`TellIntent::new_with_crash_restart`] at intent construction time and
/// must return a fresh `TellIntent` that re-sends the same command to the
/// same target, or `None` if reconstruction is not possible.
///
/// Registered on [`crate::SagaProps`] via
/// [`crate::SagaProps::with_crash_restart_factory`].
pub(crate) type CrashRestartFactory = Arc<dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync>;

pub(crate) type RouteFn<E> = Arc<dyn Fn(&E) -> Option<SagaId> + Send + Sync>;

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
    /// Monotonic sequence cursor for the saga's own stream.  Owned by the
    /// saga process; outbox executor children claim a fresh sequence by sending
    /// `AppendTerminalAndClaim` to the saga's own mailbox.
    pub(crate) sequence: u64,
    pub(crate) retry_policy: RetryPolicy,
    /// Registry of in-flight [`TellIntent`]s.  Populated by the interpreter
    /// before spawning each outbox executor child; the entry is removed by the
    /// `AppendTerminalAndClaim` handler once a terminal marker is durably
    /// appended.  On supervised restart the replay path checks this registry to
    /// re-dispatch any pending tells.
    pub(crate) pending_intents: HashMap<u64, TellIntent>,
    /// Optional factory for crash-restart re-dispatch.
    ///
    /// When the saga process starts after a full OS-process crash, the
    /// in-memory `pending_intents` registry is gone.  If this factory is
    /// `Some`, the replay path calls it with the crash-restart bytes stored
    /// in the `TellRequested` payload to reconstruct the [`TellIntent`] and
    /// spawn the retry executor.  Registered via
    /// [`crate::SagaProps::with_crash_restart_factory`].
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    /// Accumulator of `tell_id`s whose outbox executor durably appended a
    /// `TellFailed` terminal marker since the last `Saga::handle` call.
    ///
    /// Drained before each `handle` invocation and passed as
    /// `SagaContext::failed_tell_ids`.
    pub(crate) failed_tell_ids: Vec<u64>,
    /// Set to `true` when `SagaEffect::End` is interpreted while outbox
    /// executors are still in-flight.  The actual `stop_self()` is deferred
    /// until every spawned executor settles via `AppendTerminalAndClaim`
    /// (whether the terminal append succeeds or fails), at which point
    /// `pending_executor_count` reaches zero and `stop_self()` fires.
    pub(crate) pending_end: bool,
    /// Count of outbox executors that have been spawned and have not yet
    /// reported completion.  Incremented in the interpreter's `persist_batch`
    /// for every spawned executor.  Decremented by the `AppendTerminalAndClaim`
    /// handler.  The deferred-stop condition is
    /// `pending_end && pending_executor_count == 0`.
    pub(crate) pending_executor_count: usize,
    pub(crate) upstream_config: DurableSubscription<EventEnvelope<S::SubscribedEvent>>,
    pub(crate) upstream_cursor: SequenceCursor,
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let factory_ref = self.crash_restart_factory.as_ref().map(|f| f.as_ref());
        let scan_failed = replay_and_redispatch(
            &self.saga_id,
            &mut self.state,
            self.codec.as_ref(),
            &self.store,
            &mut self.sequence,
            &mut self.pending_intents,
            factory_ref,
            self.retry_policy.clone(),
            ctx,
        )
        .await;
        if !scan_failed.is_empty() {
            self.failed_tell_ids.extend(scan_failed);
        }
        // Each entry surviving in pending_intents after replay corresponds to a
        // re-spawned outbox executor.  Initialise the counter so the deferred-stop
        // condition works correctly if End is not encountered in this run, and
        // also for the unusual case where a supervised restart is followed by End.
        self.pending_executor_count = self.pending_intents.len();

        self.upstream_config
            .spawn_child(ctx, self.upstream_cursor.clone())
            .await;
    }
}

impl<S: Saga> Receive<EventEnvelope<S::SubscribedEvent>> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<S::SubscribedEvent>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let Some(target_id) = (self.route_fn)(&msg.event) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        let current_sequence = self.sequence;
        // Drain accumulated failed tell IDs and surface them to this handle call.
        let drained_failed = std::mem::take(&mut self.failed_tell_ids);
        let mut saga_ctx = SagaContext::new(
            self.saga_id.clone(),
            current_sequence,
            msg.aggregate_id.clone(),
            msg.sequence,
            jiff::Timestamp::now(),
            drained_failed,
        );
        let effect = match self.state.handle(msg.event, &mut saga_ctx).await {
            Ok(effect) => effect,
            Err(e) => {
                tracing::warn!(error = %e, "saga handle failed");
                return Ok(());
            }
        };

        let mut ictx = InterpreterCtx {
            state: &mut self.state,
            saga_id: self.saga_id.clone(),
            sequence: &mut self.sequence,
            store: Arc::clone(&self.store),
            codec: Arc::clone(&self.codec),
            retry_policy: self.retry_policy.clone(),
            process_ctx: ctx,
            pending_intents: &mut self.pending_intents,
            pending_end: &mut self.pending_end,
            pending_executor_count: &mut self.pending_executor_count,
        };
        let _ = run_saga_effect(effect, &mut ictx).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal self-messages
// ---------------------------------------------------------------------------

/// Append a terminal outbox marker, advance the sequence cursor, and perform
/// the in-memory cleanup for a settled tell — all atomically inside the saga's
/// single-threaded loop.
///
/// Sent (fire-and-forget) by an [`OutboxExecutorProcess`] once its retry loop
/// reaches a terminal outcome, immediately before the executor stops itself.
/// The append and the cursor advance happen together: `sequence` is only
/// committed when `EventStore::append` succeeds, so a store failure never skips
/// a sequence number.
///
/// On a durable append the handler removes the `pending_intents` entry (and, for
/// `TerminalKind::Failed`, records the `tell_id` in `failed_tell_ids` so the
/// next [`crate::Saga::handle`] call can observe it). On a store failure the
/// entry is **kept** so a subsequent supervised restart can re-dispatch the
/// tell. Either way `pending_executor_count` is decremented and, if `End` was
/// deferred while executors were in-flight, `stop_self` fires once the last
/// executor settles.
///
/// [`OutboxExecutorProcess`]: crate::process::outbox_executor::OutboxExecutorProcess
pub(crate) struct AppendTerminalAndClaim {
    pub(crate) tell_id: u64,
    pub(crate) kind: TerminalKind,
}

/// Core logic for [`AppendTerminalAndClaim`], extracted so it can be unit-tested
/// without constructing a full [`ProcessContext`].
///
/// Attempts to append a terminal marker at `*sequence + 1`.  Only advances
/// `*sequence` when the store call succeeds.
async fn append_terminal_and_claim(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    kind: TerminalKind,
    tell_id: u64,
) -> bool {
    let candidate = *sequence + 1;
    let appended = OutboxAppender::append_terminal(store, saga_id, candidate, kind, tell_id).await;
    if appended {
        *sequence = candidate;
    }
    appended
}

impl<S: Saga> Receive<AppendTerminalAndClaim> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: AppendTerminalAndClaim,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let appended = append_terminal_and_claim(
            &self.store,
            &self.saga_id,
            &mut self.sequence,
            msg.kind,
            msg.tell_id,
        )
        .await;

        if appended {
            // Durable terminal append: drop the in-flight intent so a future
            // supervised restart does not re-dispatch an already-settled tell.
            self.pending_intents.remove(&msg.tell_id);
            if matches!(msg.kind, TerminalKind::Failed) {
                // Surface the failure to the next `Saga::handle` via
                // `SagaContext::failed_tell_ids`.
                self.failed_tell_ids.push(msg.tell_id);
            }
        } else {
            // Store failure: keep the `pending_intents` entry intact so a
            // subsequent supervised restart can re-dispatch the tell via the
            // in-memory path rather than falling back to synthetic TellFailed.
            tracing::debug!(
                tell_id = msg.tell_id,
                "saga outbox terminal append failed; \
                 pending_intents entry preserved for supervised-restart re-dispatch"
            );
        }

        self.pending_executor_count = self.pending_executor_count.saturating_sub(1);
        // If End was deferred because outbox executors were still in-flight,
        // stop now that the last executor has settled.
        if self.pending_end && self.pending_executor_count == 0 {
            if let Err(e) = ctx.stop_self().await {
                tracing::warn!(
                    error = %e,
                    "saga deferred End: stop_self failed after all outbox executors settled"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use futures_core::Stream;
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

    /// An `EventStore` that fails the first `append` call, then succeeds.
    struct FailOnceStore {
        has_failed: AtomicBool,
    }

    impl FailOnceStore {
        fn new() -> Self {
            Self {
                has_failed: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl EventStore for FailOnceStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            if !self.has_failed.swap(true, Ordering::SeqCst) {
                Err(AppendError::Backend("injected failure".into()))
            } else {
                Ok(AppendOutcome {
                    assigned_sequences: vec![],
                    stream_version: 0,
                })
            }
        }

        async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let stream: Pin<
                Box<
                    dyn Stream<Item = Result<nitinol_persistence::LoadedEvent, LoadError>>
                        + Send
                        + '_,
                >,
            > = Box::pin(futures_util::stream::empty());
            Ok(stream)
        }
    }

    /// Regression test: a failed `append_terminal_and_claim` must not advance
    /// the sequence cursor, so the subsequent successful call uses `old + 1`
    /// rather than `old + 2`.
    #[tokio::test]
    async fn append_terminal_does_not_advance_sequence_on_store_failure() {
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("sequence-skip-regression");
        let mut sequence: u64 = 0;

        // First call: store fails → sequence must stay at 0.
        let ok = append_terminal_and_claim(&store, &saga_id, &mut sequence, TerminalKind::Acked, 1)
            .await;
        assert!(!ok, "must return false when EventStore::append fails");
        assert_eq!(sequence, 0, "sequence must not advance on store failure");

        // Second call: store succeeds → sequence advances to 1, not 2.
        let ok = append_terminal_and_claim(&store, &saga_id, &mut sequence, TerminalKind::Acked, 1)
            .await;
        assert!(ok, "must return true when EventStore::append succeeds");
        assert_eq!(
            sequence, 1,
            "sequence must be 1 (no gap from the failed attempt)"
        );
    }
}
