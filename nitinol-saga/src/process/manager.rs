//! The saga instance manager: one upstream subscription in front of every
//! instance of a saga type.
//!
//! The manager is the machine that interprets [`Saga::correlate`] at runtime.
//! It holds the only subscription, so an upstream record is decoded once no
//! matter how many instances the correlation fans out to, and it owns the
//! single cursor over that subscription — which makes "do not advance on
//! failure" a manager decision rather than an instance one.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{appending_system_event, DurableSubscription, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadQuery;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Terminated};
use nitinol_runtime::IdleTimeout;

use crate::dead_letter::{
    DeadLetterDispositionEvent, DispositionArbiter, DlqChildSpawn, EnqueuePolicy,
    RecordDisposition, SettleDeadLetter, SettleError,
};
use crate::error::SagaUpstreamHandlerError;
use crate::id::SagaId;
use crate::process::instance::SagaInstanceSpec;
use crate::process::saga_process::{
    CrashRestartFactory, DecodeFailureRouteFn, SagaProcess, UpstreamMessage,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;

pub(crate) struct SagaManagerProcess<S: Saga> {
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) producer: Arc<dyn Fn() -> S + Send + Sync>,
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    pub(crate) upstream: DurableSubscription<UpstreamMessage<S::SubscribedEvent>>,
    pub(crate) cursor: SequenceCursor,
    /// Passivation window handed to every instance this manager spawns.
    pub(crate) instance_idle_timeout: IdleTimeout,
    /// Resident scheduler handed to every instance this manager spawns, so its
    /// `SagaEffect::schedule` / `cancel_schedule` effects drive real timers —
    /// mirrors `SagaProps::with_scheduler` on the standalone builder.
    pub(crate) scheduler: Option<SchedulerProxy>,
    /// DLQ subscriber wiring handed to every instance this manager spawns, so
    /// each one starts its own poller over its own stream — mirrors
    /// `SagaProps::with_dead_letter_subscriber` on the standalone builder.
    pub(crate) dlq_child_spawn: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    /// Crash-restart re-dispatch factory handed to every instance this manager
    /// spawns, so a tell left pending when an instance was passivated is
    /// reconstructed from its persisted payload on revival — mirrors
    /// `SagaProps::with_crash_restart_factory` on the standalone builder.
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    /// Determines which instance owns an upstream record the codec could not
    /// decode.  `None` (unset, or the function declining) means no instance is
    /// attributed and the record is skipped — see `recv` below.
    pub(crate) decode_failure_route_fn: Option<DecodeFailureRouteFn>,
    /// The instances currently resident, keyed by the id `Saga::correlate`
    /// named.  An id absent here is not an id that never existed — it is one
    /// with no *running* process, which is why a miss replays rather than
    /// starts from scratch.
    pub(crate) instances: HashMap<SagaId, ProcessProxy<SagaProcess<S>>>,
}

impl<S: Saga> SagaManagerProcess<S> {
    /// The running instance for `saga_id`, spawning it as a child first when
    /// none is resident.
    ///
    /// A freshly spawned instance replays its own stream in `on_start`, which
    /// the runtime completes before any message reaches its mailbox — so the
    /// event that caused the spawn is delivered to restored state.
    async fn instance(
        &mut self,
        saga_id: SagaId,
        ctx: &mut ProcessContext<Self>,
    ) -> ProcessProxy<SagaProcess<S>> {
        if let Some(proxy) = self.instances.get(&saga_id) {
            return proxy.clone();
        }

        let props = SagaInstanceSpec::<S> {
            saga_id: saga_id.clone(),
            store: Arc::clone(&self.store),
            producer: Arc::clone(&self.producer),
            codec: Arc::clone(&self.codec),
            enqueue_policy: Arc::clone(&self.enqueue_policy),
            // This manager is the subscription; an instance that opened its own
            // would decode every upstream record a second time.
            upstream: None,
            crash_restart_factory: self.crash_restart_factory.clone(),
            scheduler: self.scheduler.clone(),
            // Routing a corrupt record is the manager's call, not an
            // instance-level one — see the `DecodeFailed` arm below.
            decode_failure_route_fn: None,
            dlq_child_spawn: self.dlq_child_spawn.clone(),
            #[cfg(test)]
            initial_tell_states: HashMap::new(),
        }
        .into_props()
        .with_idle_timeout(self.instance_idle_timeout);

        let proxy: ProcessProxy<SagaProcess<S>> = ctx.spawn_child(props).await;
        // `spawn_child` registers only the hierarchy watch, which never reaches
        // `on_terminated`; without this explicit watch a passivated instance
        // would linger in the registry as a proxy to a dead process.
        ctx.watch(proxy.pid()).await;
        self.instances.insert(saga_id, proxy.clone());
        proxy
    }

    /// Put `marker` onto `saga_id`'s stream through whoever owns that stream's
    /// next sequence.
    ///
    /// The whole decision — is anyone resident, and the write that follows from
    /// the answer — happens here, inside the manager's own message handling.
    /// That placement is the point: this manager is the only thing that spawns
    /// an instance (`instance`, reached from `recv`) and the only thing that
    /// retires one (`on_terminated`), and all three run on its single mailbox.
    /// So residency cannot change between reading it and acting on it, and
    /// there is no window in which "nobody is resident, write it myself" is
    /// overtaken by a lazy spawn that then believes it owns the tail.
    ///
    /// A dormant saga is *not* woken to do this: settling a dead letter is
    /// bookkeeping about a saga, not traffic for it.
    async fn settle_dead_letter(
        &mut self,
        saga_id: SagaId,
        marker: DeadLetterDispositionEvent,
    ) -> Result<(), SettleError> {
        if let Some(instance) = self.instances.get(&saga_id).cloned() {
            return match instance.ask(RecordDisposition { marker }).await {
                Ok(appended) => appended.map_err(SettleError::Append),
                // The instance stopped between the registry read and the ask —
                // `on_terminated` has not been processed yet.  Nothing was
                // written, so the operator can retry.
                Err(e) => Err(SettleError::Unreachable(e.to_string())),
            };
        }

        let tail = self.stream_tail(&saga_id).await?;
        self.store
            .append(
                saga_id.as_str(),
                vec![appending_system_event(
                    tail + 1,
                    &marker,
                    jiff::Timestamp::now(),
                )],
            )
            .await?;
        Ok(())
    }

    /// The highest sequence on `saga_id`'s stream, or 0 when it is empty.
    ///
    /// Read rather than remembered: the manager holds no sequence for a saga
    /// with no resident instance, and the store offers no tail lookup that is
    /// not a scan.
    async fn stream_tail(&self, saga_id: &SagaId) -> Result<u64, SettleError> {
        let stream: Vec<_> = self
            .store
            .load(LoadQuery::by_stream(saga_id))
            .await?
            .try_collect()
            .await?;
        Ok(stream
            .iter()
            .map(|loaded| loaded.sequence)
            .max()
            .unwrap_or(0))
    }
}

impl<S: Saga> Process for SagaManagerProcess<S> {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        self.upstream.spawn_child(ctx, self.cursor.clone()).await;
    }

    async fn on_terminated(&mut self, terminated: Terminated, _ctx: &mut ProcessContext<Self>) {
        // However an instance stopped — passivated by its idle timeout, or
        // stopped by its own handler-failure supervision — it must leave the
        // registry, so the next event for that correlation id spawns a replayed
        // successor instead of being delivered into a dead process.
        self.instances
            .retain(|_, proxy| proxy.pid() != terminated.who);
    }
}

impl<S: Saga> Receive<UpstreamMessage<S::SubscribedEvent>> for SagaManagerProcess<S> {
    type Response = ();
    /// Returning `Err` stops the `DirectPoller` (via `ask()`) from advancing the
    /// one shared cursor, so a record the addressed instance could not settle is
    /// redelivered rather than skipped.
    type Error = SagaUpstreamHandlerError;

    async fn recv(
        &mut self,
        msg: UpstreamMessage<S::SubscribedEvent>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SagaUpstreamHandlerError> {
        let correlated = match &msg {
            UpstreamMessage::Decoded { event, .. } => S::correlate(event),
            UpstreamMessage::DecodeFailed {
                aggregate_id,
                sequence,
                error,
            } => {
                // A corrupt record carries no typed event, so `Saga::correlate`
                // cannot name an owner.  `decode_failure_route_fn`, when
                // configured, resolves an owner from the wire key and sequence
                // instead — the same shape as `SagaProps::with_decode_failure_route`,
                // whose `Some`/`None` answer this reuses verbatim.  With no
                // owner resolved the manager has nowhere to record it, so it is
                // reported and — like an uncorrelated `Decoded` record below —
                // skipped rather than holding the shared cursor for every
                // instance behind it.
                let owner = self
                    .decode_failure_route_fn
                    .as_ref()
                    .and_then(|route| route(aggregate_id, *sequence));
                if owner.is_none() {
                    tracing::error!(
                        upstream_key = aggregate_id.as_str(),
                        upstream_sequence = sequence,
                        error = %error,
                        "saga manager cannot attribute an undecodable upstream record \
                         to an instance; skipping it"
                    );
                }
                owner
            }
        };

        // A record belonging to no instance is nobody's work.  Advancing past it
        // is what keeps one unclaimed record from stalling every instance behind
        // it on the shared subscription.
        let Some(saga_id) = correlated else {
            return Ok(());
        };

        let instance = self.instance(saga_id, ctx).await;
        instance.ask(msg).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                "saga manager: instance did not settle the upstream record; \
                 holding the shared cursor so it is redelivered"
            );
            SagaUpstreamHandlerError
        })
    }
}

impl<S: Saga> Receive<SettleDeadLetter> for SagaManagerProcess<S> {
    /// The outcome is the *reply*, not the handler's result.
    ///
    /// A disposition the store refused is a failed operator action, not a
    /// failing manager: raising it as `Err` would put an operator's bookkeeping
    /// write through supervision and let it disturb a fan-out that is otherwise
    /// healthy.
    type Response = Result<(), SettleError>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        msg: SettleDeadLetter,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Result<(), SettleError>, Infallible> {
        Ok(self.settle_dead_letter(msg.saga_id, msg.marker).await)
    }
}

/// Concrete [`DispositionArbiter`] holding a typed handle to one manager.
///
/// `S` is erased at the trait boundary so `DeadLetterQueue` — which is not
/// generic over the saga type — can route through a manager that is.
struct TypedDispositionArbiter<S: Saga> {
    manager: ProcessProxy<SagaManagerProcess<S>>,
}

impl<S: Saga> DispositionArbiter for TypedDispositionArbiter<S> {
    fn settle<'a>(
        &'a self,
        saga_id: &'a SagaId,
        marker: DeadLetterDispositionEvent,
    ) -> BoxFuture<'a, Result<(), SettleError>> {
        Box::pin(async move {
            let asked = self
                .manager
                .ask(SettleDeadLetter {
                    saga_id: saga_id.clone(),
                    marker,
                })
                .await;
            match asked {
                Ok(settled) => settled,
                Err(e) => Err(SettleError::Unreachable(e.to_string())),
            }
        })
    }
}

/// Construct a type-erased [`DispositionArbiter`] over `manager`.
pub(crate) fn make_disposition_arbiter<S: Saga>(
    manager: ProcessProxy<SagaManagerProcess<S>>,
) -> Arc<dyn DispositionArbiter> {
    Arc::new(TypedDispositionArbiter { manager })
}
