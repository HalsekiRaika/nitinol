use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableStreamProxy, EventEnvelope};
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use tokio::sync::Mutex;

use crate::context::SagaContext;
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::interpreter::{run_saga_effect, InterpreterCtx};
use crate::process::pending_intents::PendingIntents;
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
pub(crate) type CrashRestartFactory =
    Arc<dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync>;

pub(crate) type RouteFn<E> = Arc<dyn Fn(&E) -> Option<SagaId> + Send + Sync>;

pub struct SagaProcess<S: Saga> {
    pub(crate) state: S,
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
    /// Shared monotonic sequence cursor for the saga's own stream.
    ///
    /// Held behind an `Arc<Mutex<_>>` because the outbox retry executor (a
    /// `tokio::spawn` task) needs to claim a fresh sequence number to append
    /// its terminal `TellAcked` / `TellFailed` marker after the originating
    /// `recv` has already returned.
    pub(crate) sequence: Arc<Mutex<u64>>,
    pub(crate) retry_policy: RetryPolicy,
    /// Registry of in-flight [`TellIntent`]s.  Populated by the interpreter
    /// before spawning each outbox executor; consumed (entry removed) by the
    /// executor when it appends the terminal marker.  On supervised restart the
    /// replay path checks this registry to re-dispatch any pending tells.
    pub(crate) pending_intents: PendingIntents,
    /// Optional factory for crash-restart re-dispatch.
    ///
    /// When the saga process starts after a full OS-process crash, the
    /// in-memory [`PendingIntents`] registry is gone.  If this factory is
    /// `Some`, the replay path calls it with the crash-restart bytes stored
    /// in the `TellRequested` payload to reconstruct the [`TellIntent`] and
    /// spawn the retry executor.  Registered via
    /// [`crate::SagaProps::with_crash_restart_factory`].
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    /// Accumulator of `tell_id`s whose outbox executor durably appended a
    /// `TellFailed` terminal marker since the last `Saga::handle` call.
    ///
    /// Outbox executors push here after a successful `TellFailed` append.
    /// The replay path pre-populates it with tell_ids that had a `TellFailed`
    /// marker in the event history on restart.  `recv` drains it before each
    /// `handle` invocation and passes the drained slice as
    /// `SagaContext::failed_tell_ids`.
    pub(crate) failed_tell_ids: Arc<Mutex<Vec<u64>>>,
    /// Keeps the upstream `DurableStream` alive for exactly as long as this
    /// `SagaProcess` is alive.  Tied to the process, not to the external
    /// `SagaProxy` handle, so that dropping all `SagaProxy` clones never
    /// silently kills the poller while the saga is still running.
    pub(crate) _ds_keepalive: Arc<DurableStreamProxy<EventEnvelope<S::SubscribedEvent>>>,
}

impl<S: Saga> Process for SagaProcess<S> {
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        // Convert Option<Arc<dyn Fn(...)>> to Option<&dyn Fn(...)> for replay.
        let factory_ref = self.crash_restart_factory.as_ref().map(|f| f.as_ref());
        let scan_failed = replay_and_redispatch(
            &self.saga_id,
            &mut self.state,
            self.codec.as_ref(),
            &self.store,
            &self.sequence,
            &self.pending_intents,
            factory_ref,
            self.retry_policy.clone(),
            Arc::clone(&self.failed_tell_ids),
        )
        .await;
        if !scan_failed.is_empty() {
            self.failed_tell_ids.lock().await.extend(scan_failed);
        }
    }
}

impl<S: Saga> Receive<EventEnvelope<S::SubscribedEvent>> for SagaProcess<S> {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<S::SubscribedEvent>,
        ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        let Some(target_id) = (self.route_fn)(&msg.event) else {
            return Ok(());
        };
        if target_id != self.saga_id {
            return Ok(());
        }

        let current_sequence = *self.sequence.lock().await;
        // Drain accumulated failed tell IDs and surface them to this handle call.
        let drained_failed = {
            let mut guard = self.failed_tell_ids.lock().await;
            std::mem::take(&mut *guard)
        };
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
            sequence: Arc::clone(&self.sequence),
            store: Arc::clone(&self.store),
            codec: Arc::clone(&self.codec),
            retry_policy: self.retry_policy.clone(),
            process_ctx: ctx,
            pending_intents: self.pending_intents.clone(),
            failed_tell_ids: Arc::clone(&self.failed_tell_ids),
        };
        let _ = run_saga_effect(effect, &mut ictx).await;
        Ok(())
    }
}
