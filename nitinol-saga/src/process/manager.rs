//! The saga instance manager: the source partitions in front of every instance
//! of a saga type.
//!
//! The manager is the machine that interprets [`Saga::correlate`] at runtime.
//! Its source partitions are the only readers of the declared sources, so an
//! observed record is decoded once per partition no matter how many instances
//! the correlation fans out to, and each partition owns its source's offset
//! for the instances that fall in it — which makes "do not move past a record
//! that is not settled" a partition decision rather than an instance one.
//!
//! Each partition of each declared source resumes from the higher of the
//! source's floor and the offset it recorded, so a restart reads on from where
//! the last one settled instead of reading the source again from its floor.
//! The offset is a cache of what the journal already says: an instance drops
//! every record its own stream names as consumed, so an offset lost or carried
//! backwards costs a re-read and never a second decision.
//!
//! The declared sources stay independent of one another, and so do the
//! partitions of one: an offset each, no order between them, and a stalled
//! partition holding up nothing but itself.

use std::collections::HashMap;
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::Arc;

use nitinol_eventsource::system::ContingentSystem;
use nitinol_persistence::store::Journal;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive, Terminated};
use nitinol_runtime::IdleTimeout;

use crate::delivery::SagaDeliveryId;
use crate::error::SagaUpstreamHandlerError;
use crate::id::SagaId;
use crate::process::instance::{SagaInstanceSpec, SagaProducer, ScheduleDelivery};
use crate::process::manager_offsets::SagaManagerOffsets;
use crate::process::saga_process::{
    FireScheduled, SagaProcess, UpstreamMessage, UpstreamSettlement,
};
use crate::process::source_partition::{SagaSourcePartition, UpstreamSource};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;

pub(crate) struct SagaManagerProcess<S: Saga> {
    pub(crate) store: Journal,
    pub(crate) producer: SagaProducer<S>,
    /// The system this manager was wired from.
    ///
    /// Handed to every instance it spawns, so an instance reaches the host's
    /// resources without a reference to the system being captured outside the
    /// spawn.
    pub(crate) host: ContingentSystem,
    /// The declared sources, each read by one partition per partition of the
    /// host, shared by every instance of that partition.
    ///
    /// Each partition resumes from the higher of the floor its
    /// [`Subscription`](crate::Subscription) named and the offset it recorded —
    /// no instance's consumed position speaks for either. An instance with a
    /// pending deadline and nothing new upstream is brought back by the
    /// deadline index, not by reading the source again.
    pub(crate) upstream: Vec<UpstreamSource<S::Subscribe>>,
    /// The saga-offset slot every source partition records its progress in —
    /// the system's, the one the delivery host records in.
    pub(crate) offsets: SagaManagerOffsets,
    /// How many partitions each source is read by: the count the delivery
    /// host and the deadline index are spread over too.
    pub(crate) partitions: NonZeroUsize,
    /// The delivery host's identifier, which the source partitions' offsets are
    /// filed under too.
    pub(crate) host_id: SagaDeliveryId,
    /// Passivation window handed to every instance this manager spawns.
    pub(crate) instance_idle_timeout: IdleTimeout,
    /// The resident timer core handed to every instance this manager spawns,
    /// resolved at the manager's spawn boundary from the saga's own alphabet —
    /// which is the whole of a saga's deadline wiring, however many instances
    /// the correlation fans out to.
    pub(crate) scheduler: Option<SchedulerProxy>,
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
            store: self.store.clone(),
            producer: Arc::clone(&self.producer),
            host: self.host.clone(),
            scheduler: self.scheduler.clone(),
            // A deadline this instance registers outlives it: the firing is
            // addressed here, with the instance's id, so a deadline reached
            // after passivation brings the instance back instead of landing in
            // a stopped process.
            delivery: ScheduleDelivery::through(ctx.self_proxy().clone()),
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
        // Every partition there is. This range is the one place a host that
        // took only some of the partitions would name its share.
        let owned = 0..self.partitions.get();
        // One reader per declared source and partition, each from its own
        // offset, so a partition whose record is stalled holds up only itself.
        for source in &self.upstream {
            for partition in owned.clone() {
                let props = SagaSourcePartition::new(
                    source.clone(),
                    partition,
                    self.partitions,
                    &self.host_id,
                    self.store.clone(),
                    self.offsets.clone(),
                    ctx.self_proxy().clone(),
                )
                .into_props();
                ctx.spawn_child(props).await;
            }
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
    /// The addressed instance's answer, handed back to the partition that read
    /// the record so it decides how far its offset may move.
    type Response = UpstreamSettlement;
    /// Returning `Err` keeps the source partition (via `ask()`) from reading
    /// past the record, so a record the addressed instance could not answer for
    /// is redelivered rather than skipped.
    type Error = SagaUpstreamHandlerError;

    async fn recv(
        &mut self,
        msg: UpstreamMessage<S::Subscribe>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<UpstreamSettlement, SagaUpstreamHandlerError> {
        let instance = self.instance(msg.saga_id.clone(), ctx).await;
        instance.ask(msg).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                "saga manager: instance did not settle the observed record; \
                 holding that source partition so it is redelivered"
            );
            SagaUpstreamHandlerError
        })
    }
}

/// Ask for one of this manager's instances to be resident.
///
/// Sent by the deadline index when a deadline the journal holds for `saga_id`
/// falls due. It carries no firing: an instance brought back replays its own
/// stream, re-arms the deadline on the resident scheduler, and the scheduler's
/// firing reaches it through [`DeliverScheduled`] — the one road a deadline
/// takes to `handle`.
pub(crate) struct ActivateInstance {
    pub(crate) saga_id: SagaId,
}

impl<S: Saga> Receive<ActivateInstance> for SagaManagerProcess<S> {
    type Response = ();
    /// One instance that cannot be brought back must not put the manager that
    /// fronts every other one through supervision.
    type Error = Infallible;

    /// Activation on miss, and nothing for a resident instance.
    ///
    /// A stale proxy is not evicted here, unlike [`DeliverScheduled`]: nothing
    /// is handed to it, so nothing is lost by its being stale, and the index
    /// asks again until the journal settles the deadline — by when the
    /// instance's `Terminated` has cleared it from the registry and the request
    /// spawns a successor.
    async fn recv(
        &mut self,
        msg: ActivateInstance,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.instance(msg.saga_id, ctx).await;
        Ok(())
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
