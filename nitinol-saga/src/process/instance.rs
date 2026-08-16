//! The resolved ingredients of one saga instance process.

use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::Props;

use crate::dead_letter::{DlqChildSpawn, EnqueuePolicy};
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::saga_process::{
    CrashRestartFactory, DecodeFailureRouteFn, Lifecycle, OwnedUpstream, SagaProcess, TellState,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;

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
    pub(crate) codec: Arc<dyn ErasedCodec<S::Event>>,
    pub(crate) enqueue_policy: Arc<dyn EnqueuePolicy>,
    pub(crate) upstream: Option<OwnedUpstream<S::SubscribedEvent>>,
    pub(crate) crash_restart_factory: Option<CrashRestartFactory>,
    pub(crate) scheduler: Option<SchedulerProxy>,
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
                codec: Arc::clone(&self.codec),
                decode_failure_route_fn: self.decode_failure_route_fn.clone(),
                sequence: 0,
                retry_policy: RetryPolicy::default(),
                tell_states,
                crash_restart_factory: self.crash_restart_factory.clone(),
                lifecycle: Lifecycle::Running,
                upstream: self.upstream.clone(),
                scheduler: self.scheduler.clone(),
                enqueue_policy: Arc::clone(&self.enqueue_policy),
                dlq_child_spawn: self.dlq_child_spawn.clone(),
            }
        })
    }
}
