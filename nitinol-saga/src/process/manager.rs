//! The saga instance manager: one subscription per declared source in front of
//! every instance of a saga type.
//!
//! The manager is the machine that interprets [`Saga::correlate`] at runtime.
//! It holds the only subscriptions, so an observed record is decoded once no
//! matter how many instances the correlation fans out to, and it owns the one
//! cursor of each source — which makes "do not advance on failure" a manager
//! decision rather than an instance one.
//!
//! Sources stay independent under a manager exactly as they are under a
//! standalone saga: one cursor each, no order between them, and a stalled source
//! holding up nothing but itself.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use nitinol_eventsource::SystemEvent;
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
use crate::observability::report_system_event;
use crate::process::instance::{SagaInstanceSpec, SagaProducer, ScheduleDelivery};
use crate::process::saga_process::{
    CrashRestartFactory, FireScheduled, OwnedUpstream, SagaProcess, UpstreamMessage,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;

pub(crate) struct SagaManagerProcess<S: Saga> {
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) producer: SagaProducer<S>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    /// One subscription per declared source, shared by every instance the
    /// manager fronts.
    ///
    /// Each starts exactly where it was configured, unlike the cursor of a saga
    /// that owns its subscriptions: a shared cursor is spoken for by no single
    /// instance's consumed position, and the sweep from the configured position
    /// is what activates the instances that are not resident.  Each instance
    /// still drops the records it has already consumed, so the sweep costs no
    /// second decision.
    pub(crate) upstream: Vec<OwnedUpstream<S::Subscribe>>,
    /// Passivation window handed to every instance this manager spawns.
    pub(crate) instance_idle_timeout: IdleTimeout,
    /// The resident timer core handed to every instance this manager spawns,
    /// resolved at the manager's spawn boundary from the saga's own alphabet —
    /// so a manager's instances get their deadlines on the same terms a
    /// standalone saga does.
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
    ///
    /// A hit in `instances` is returned without a liveness check: the id's
    /// `Terminated` notification may still be queued behind the message this
    /// call is serving.  A caller that needs the proxy to actually be alive
    /// must treat a `tell` / `ask` failure as "stale, not just unreachable" —
    /// evict it from `instances` and call this again — the way
    /// `Receive<DeliverScheduled>::recv` does.
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
            enqueue_policy: Arc::clone(&self.enqueue_policy),
            // This manager holds the subscriptions; an instance that opened its
            // own would decode every observed record a second time.
            upstream: Vec::new(),
            crash_restart_factory: self.crash_restart_factory.clone(),
            scheduler: self.scheduler.clone(),
            // A deadline this instance registers outlives it: the firing is
            // addressed here, with the instance's id, so a deadline reached
            // after passivation brings the instance back instead of landing in
            // a stopped process.
            delivery: ScheduleDelivery::ThroughTheManager(ctx.self_proxy().clone()),
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
                vec![marker.appending(tail + 1, jiff::Timestamp::now())],
            )
            .await?;
        report_system_event!(
            DeadLetterDispositionEvent::EVENT_TYPE,
            marker.arm(),
            saga_id.as_str(),
            dead_letter_sequence = marker.dead_letter_sequence,
        );
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
        // One poller per declared source, each from its own configured position:
        // the fan-out observes exactly what a standalone saga would, and a
        // source whose upstream has stalled holds up only itself.
        for source in self.upstream.clone() {
            source.config.spawn_child(ctx, source.cursor).await;
        }
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

impl<S: Saga> Receive<UpstreamMessage<S::Subscribe>> for SagaManagerProcess<S> {
    type Response = ();
    /// Returning `Err` stops that source's `DirectPoller` (via `ask()`) from
    /// advancing its shared cursor, so a record the addressed instance could not
    /// settle is redelivered rather than skipped.
    type Error = SagaUpstreamHandlerError;

    async fn recv(
        &mut self,
        msg: UpstreamMessage<S::Subscribe>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SagaUpstreamHandlerError> {
        // A record belonging to no instance is nobody's work.  Advancing past it
        // is what keeps one unclaimed record from stalling every instance behind
        // it on that source's shared subscription.
        let Some(saga_id) = S::correlate(&msg.input) else {
            return Ok(());
        };

        let instance = self.instance(saga_id, ctx).await;
        instance.ask(msg).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                "saga manager: instance did not settle the observed record; \
                 holding that source's shared cursor so it is redelivered"
            );
            SagaUpstreamHandlerError
        })
    }
}

/// A deadline's firing addressed to one of this manager's instances.
///
/// What the resident scheduler holds for such a deadline is this manager and
/// the instance's [`SagaId`] — never a proxy to the incarnation that
/// registered it, which may have been passivated long before the deadline was
/// reached.
pub(crate) struct DeliverScheduled {
    pub(crate) saga_id: SagaId,
    pub(crate) fire: FireScheduled,
}

impl<S: Saga> Receive<DeliverScheduled> for SagaManagerProcess<S> {
    type Response = ();
    /// A firing the instance could not be handed is reported, not raised: the
    /// deadline of one instance must not put a manager that fronts every other
    /// one through supervision.
    type Error = Infallible;

    async fn recv(
        &mut self,
        msg: DeliverScheduled,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        // Activation on miss: an instance that is no longer resident is
        // spawned and replays its own stream before the firing reaches it, so
        // what handles the firing is the saga's restored state rather than a
        // fresh one.
        let instance = self.instance(msg.saga_id.clone(), ctx).await;
        if instance.tell(msg.fire.clone()).await.is_err() {
            // `tell` fails only when the mailbox is closed, which means the
            // proxy `instance()` returned was already stale: its incarnation
            // had stopped, but this manager had not yet processed the
            // `Terminated` that would have dropped it from `instances` (read
            // and act happen on this same mailbox, but the watch notification
            // is a separate message that can still be queued behind this
            // one). Evicting it and re-running activation on miss matters
            // here specifically because this is the only delivery attempt —
            // the scheduler already removed the due entry with `take_due()`.
            self.instances.remove(&msg.saga_id);
            let revived = self.instance(msg.saga_id, ctx).await;
            if let Err(e) = revived.tell(msg.fire).await {
                tracing::warn!(
                    error = %e,
                    "saga manager: the restored instance did not accept a timer firing; \
                     the unfired schedule marker on its stream re-arms it on the next activation"
                );
            }
        }
        Ok(())
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
