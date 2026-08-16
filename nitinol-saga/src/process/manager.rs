//! The saga instance manager: one upstream subscription in front of every
//! instance of a saga type.
//!
//! The manager is the machine that interprets [`Saga::correlate`] at runtime.
//! It holds the only subscription, so an upstream record is decoded once no
//! matter how many instances the correlation fans out to, and it owns the
//! single cursor over that subscription — which makes "do not advance on
//! failure" a manager decision rather than an instance one.

use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableSubscription, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Terminated};
use nitinol_runtime::IdleTimeout;

use crate::dead_letter::{DlqChildSpawn, EnqueuePolicy};
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
