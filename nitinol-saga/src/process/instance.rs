//! The resolved ingredients of one saga instance process.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{ProcessContext, ProcessProxy};
use nitinol_runtime::Props;

use crate::consumed::ConsumedPositions;
use crate::dead_letter::{DlqChildSpawn, EnqueuePolicy};
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::manager::{DeliverScheduled, SagaManagerProcess};
use crate::process::saga_process::{
    CrashRestartFactory, DecodeFailureRouteFn, FireScheduled, Lifecycle, OwnedUpstream,
    SagaProcess, TellState,
};
use crate::saga::Saga;
use crate::scheduler::{DispatchFn, SchedulerProxy, TimerName};

/// How a deadline's firing reaches the saga instance that registered it.
///
/// A deadline belongs to the instance's identity, not to the incarnation that
/// armed it: it may be hours out, and the instance that set it may be long
/// gone by then.  Which of these two an instance was spawned with is therefore
/// also what decides whether its deadlines survive it stopping.
pub(crate) enum ScheduleDelivery<S: Saga> {
    /// Nothing fronts this instance, so a firing is addressed to the process
    /// itself and a deadline that outlived it would have nowhere to land.
    ToTheInstance,
    /// A manager fronts this instance, so a firing is addressed to the manager
    /// together with the instance's [`SagaId`] — never to a proxy for the
    /// incarnation that registered it.  The manager restores an instance that
    /// is no longer resident before handing the firing over, which is what
    /// lets a deadline outlive passivation.
    ThroughTheManager(ProcessProxy<SagaManagerProcess<S>>),
}

impl<S: Saga> Clone for ScheduleDelivery<S> {
    fn clone(&self) -> Self {
        match self {
            ScheduleDelivery::ToTheInstance => ScheduleDelivery::ToTheInstance,
            ScheduleDelivery::ThroughTheManager(manager) => {
                ScheduleDelivery::ThroughTheManager(manager.clone())
            }
        }
    }
}

impl<S: Saga> ScheduleDelivery<S> {
    /// Build the type-erased dispatch the resident scheduler invokes when the
    /// deadline is reached.
    ///
    /// `ctx` is read here rather than captured, so the dispatch a
    /// manager-fronted instance registers holds the manager and the `SagaId`
    /// alone — the firing has to survive this incarnation.
    pub(crate) fn dispatch(
        &self,
        saga_id: &SagaId,
        ctx: &ProcessContext<SagaProcess<S>>,
        name: TimerName,
        payload: Bytes,
    ) -> DispatchFn {
        match self {
            ScheduleDelivery::ToTheInstance => {
                let proxy = ctx.self_proxy().clone();
                Arc::new(move || {
                    let proxy = proxy.clone();
                    let fire = FireScheduled {
                        name: name.clone(),
                        payload: payload.clone(),
                    };
                    Box::pin(async move {
                        if let Err(e) = proxy.tell(fire).await {
                            tracing::warn!(
                                error = ?e,
                                "scheduler failed to deliver a timer firing to the saga; \
                                 it may be stopping"
                            );
                        }
                    }) as BoxFuture<'static, ()>
                })
            }
            ScheduleDelivery::ThroughTheManager(manager) => {
                let manager = manager.clone();
                let saga_id = saga_id.clone();
                Arc::new(move || {
                    let manager = manager.clone();
                    let deliver = DeliverScheduled {
                        saga_id: saga_id.clone(),
                        fire: FireScheduled {
                            name: name.clone(),
                            payload: payload.clone(),
                        },
                    };
                    Box::pin(async move {
                        if let Err(e) = manager.tell(deliver).await {
                            tracing::warn!(
                                error = ?e,
                                "scheduler failed to hand a timer firing to the saga manager; \
                                 it may be stopping"
                            );
                        }
                    }) as BoxFuture<'static, ()>
                })
            }
        }
    }

    /// Does a firing that arrives after this instance has stopped still reach
    /// it?
    ///
    /// Only a manager can answer yes: it restores the instance from its own
    /// stream.  Where the answer is no, a deadline left armed past the
    /// instance's stop can only fire into nothing.
    pub(crate) fn restores_the_instance(&self) -> bool {
        match self {
            ScheduleDelivery::ToTheInstance => false,
            ScheduleDelivery::ThroughTheManager(_) => true,
        }
    }
}

/// Everything a single saga instance needs, resolved once at its spawn
/// boundary.
///
/// Both spawn paths build this: [`SagaProps`](crate::SagaProps) for a saga that
/// owns its subscription, and
/// [`SagaManagerProps`](crate::SagaManagerProps) for each correlation id it
/// lazily spawns.  Routing every instance through one construction is what lets
/// the manager's children be the same kind of saga the standalone builder
/// produces, differing only in the values recorded here.
pub(crate) struct SagaInstanceSpec<S: Saga> {
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) producer: Arc<dyn Fn() -> S + Send + Sync>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    pub(crate) upstream: Option<OwnedUpstream<S::SubscribedEvent>>,
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    pub(crate) scheduler: Option<SchedulerProxy>,
    pub(crate) delivery: ScheduleDelivery<S>,
    pub(crate) decode_failure_route_fn: Option<DecodeFailureRouteFn>,
    pub(crate) dlq_child_spawn: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    #[cfg(test)]
    pub(crate) initial_tell_states: HashMap<u64, TellState>,
}

impl<S: Saga> SagaInstanceSpec<S> {
    /// Declare the runtime process for this instance.
    ///
    /// The ingredients are captured by the producer closure rather than moved
    /// out of it, because the runtime rebuilds the process from that closure on
    /// every supervised restart.
    pub(crate) fn into_props(self) -> Props<SagaProcess<S>> {
        Props::new(move || {
            #[cfg(not(test))]
            let tell_states: HashMap<u64, TellState> = HashMap::new();
            #[cfg(test)]
            let tell_states: HashMap<u64, TellState> = self.initial_tell_states.clone();
            SagaProcess::<S> {
                state: (self.producer)(),
                saga_id: self.saga_id.clone(),
                store: Arc::clone(&self.store),
                decode_failure_route_fn: self.decode_failure_route_fn.clone(),
                sequence: 0,
                // Both are restored from the saga's own stream in `on_start`:
                // a freshly built process knows nothing of what it consumed.
                consumed: ConsumedPositions::default(),
                retry_policy: RetryPolicy::default(),
                tell_states,
                crash_restart_factory: self.crash_restart_factory.clone(),
                lifecycle: Lifecycle::Running,
                upstream: self.upstream.clone(),
                scheduler: self.scheduler.clone(),
                delivery: self.delivery.clone(),
                enqueue_policy: Arc::clone(&self.enqueue_policy),
                dlq_child_spawn: self.dlq_child_spawn.clone(),
            }
        })
    }
}
