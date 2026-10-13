//! The resolved ingredients of one saga instance process.

use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use nitinol_eventsource::system::SystemHandle;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::Props;

use crate::components::SagaComponents;
use crate::consumed::ConsumedPositions;
use crate::id::SagaId;
use crate::process::manager::{DeliverScheduled, SagaManagerProcess};
use crate::process::saga_process::{FireScheduled, Lifecycle, SagaProcess};
use crate::retry_policy::RetryPolicy;
use crate::saga::Saga;
use crate::scheduler::{DispatchFn, SchedulerProxy, TimerName};

/// How a deadline's firing reaches the saga instance that registered it: the
/// manager that fronts it.
///
/// A deadline belongs to the instance's identity, not to the incarnation that
/// armed it — it may be hours out, and the instance that set it may be long
/// passivated by then.  So a firing is addressed to the manager together with
/// the instance's [`SagaId`], never to a proxy for the incarnation that
/// registered it: the manager restores an instance that is no longer resident
/// before handing the firing over, which is what lets a deadline outlive
/// passivation.
///
/// There is one route and no choice to make here.  Every saga instance is a
/// manager's child — a saga whose [`Saga::correlate`](crate::Saga::correlate)
/// always answers the same id is that manager with one instance — so there is
/// no shape in which a firing would have to be addressed to the instance
/// process itself and could then be lost with it.
pub(crate) struct ScheduleDelivery<S: Saga> {
    manager: ProcessProxy<SagaManagerProcess<S>>,
}

impl<S: Saga> Clone for ScheduleDelivery<S> {
    fn clone(&self) -> Self {
        Self {
            manager: self.manager.clone(),
        }
    }
}

impl<S: Saga> ScheduleDelivery<S> {
    /// Address the firings of this instance's deadlines to `manager`.
    pub(crate) fn through(manager: ProcessProxy<SagaManagerProcess<S>>) -> Self {
        Self { manager }
    }

    /// Build the type-erased dispatch the resident scheduler invokes when the
    /// deadline is reached.
    ///
    /// What the closure holds is the manager and the `SagaId` alone — nothing
    /// of the incarnation that registered the deadline, which the firing has to
    /// survive.
    pub(crate) fn dispatch(
        &self,
        saga_id: &SagaId,
        name: TimerName,
        message_type: String,
        message_codec: String,
        payload: Bytes,
    ) -> DispatchFn {
        let manager = self.manager.clone();
        let saga_id = saga_id.clone();
        Arc::new(move || {
            let manager = manager.clone();
            let deliver = DeliverScheduled {
                saga_id: saga_id.clone(),
                fire: FireScheduled {
                    name: name.clone(),
                    message_type: message_type.clone(),
                    message_codec: message_codec.clone(),
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

/// Everything a single saga instance needs, resolved once at its spawn
/// boundary.
///
/// Built by the manager for each correlation id it lazily spawns, which is the
/// one place an instance comes from: a saga whose
/// [`Saga::correlate`](crate::Saga::correlate) always answers the same id is
/// the same construction with one id in it.
pub(crate) struct SagaInstanceSpec<S: Saga> {
    pub(crate) saga_id: SagaId,
    pub(crate) store: Arc<dyn EventStore>,
    /// How one incarnation of the saga is built, from the capabilities the
    /// framework resolved for it.
    ///
    /// Called once per incarnation rather than once per instance, because a
    /// capability is bound to the incarnation that holds it: the pending-deadline
    /// view a `Deadline` reads belongs to the process that writes it.
    pub(crate) producer: SagaProducer<S>,
    /// The system this instance is hosted by, where it was wired from one.
    ///
    /// Handed to every incarnation through
    /// [`SagaComponents::system`](crate::SagaComponents::system), so a saga
    /// reaches its host's resources without one being captured outside the
    /// spawn. It is a handle: holding it keeps the host's process system and
    /// registry alive, and neither holds a saga instance back.
    pub(crate) host: Option<SystemHandle>,
    pub(crate) scheduler: Option<SchedulerProxy>,
    pub(crate) delivery: ScheduleDelivery<S>,
}

/// How a saga instance is built: the capabilities in, the saga out.
///
/// Shared because the runtime rebuilds the process from it on every supervised
/// restart, and a manager hands the same one to every instance it spawns.
pub(crate) type SagaProducer<S> = Arc<dyn Fn(SagaComponents<S>) -> S + Send + Sync>;

impl<S: Saga> SagaInstanceSpec<S> {
    /// Declare the runtime process for this instance.
    ///
    /// The ingredients are captured by the producer closure rather than moved
    /// out of it, because the runtime rebuilds the process from that closure on
    /// every supervised restart.
    pub(crate) fn into_props(self) -> Props<SagaProcess<S>> {
        Props::new(move || {
            // One cell per incarnation, shared between the saga's own `Deadline`
            // and the process that writes it: the saga reads the journal's
            // answer, and only the process can put one there.
            let pending_deadlines = crate::components::PendingDeadlineCell::default();
            SagaProcess::<S> {
                state: (self.producer)(SagaComponents::new(
                    Arc::clone(&pending_deadlines),
                    self.host.clone(),
                )),
                saga_id: self.saga_id.clone(),
                store: Arc::clone(&self.store),
                sequence: 0,
                // Both are restored from the saga's own stream in `on_start`:
                // a freshly built process knows nothing of what it consumed.
                consumed: ConsumedPositions::default(),
                retry_policy: RetryPolicy::default(),
                lifecycle: Lifecycle::Running,
                scheduler: self.scheduler.clone(),
                pending_deadlines,
                delivery: self.delivery.clone(),
            }
        })
    }
}
