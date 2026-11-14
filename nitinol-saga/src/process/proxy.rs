//! The user-facing handles to a running saga: the one a hosted declaration
//! hands back, and the manager half it names.
//!
//! The runtime `ProcessProxy<_>` is wrapped so the user never sees the
//! `Process` trait or the internal process types.  The manager handle exposes
//! only `pid()` and `stop()` for ad-hoc control — sagas are reactive and are
//! driven by their subscriptions, not by direct calls from application code.

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;

use crate::deadline_index::SagaDeadlineIndexProxy;
use crate::delivery::SagaDeliveryProxy;
use crate::process::manager::SagaManagerProcess;
use crate::saga::Saga;

/// A typed handle to a running saga instance manager.
///
/// The manager's source partitions — one per declared source and partition —
/// and every instance it spawned are runtime children of the manager process, so its
/// lifetime bounds the whole fan-out. Dropping every copy of this handle does
/// not affect them — only stopping the manager (e.g. via [`Self::stop`] or
/// supervision) does.
pub struct SagaManagerProxy<S: Saga> {
    inner: ProcessProxy<SagaManagerProcess<S>>,
}

impl<S: Saga> Clone for SagaManagerProxy<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Saga> SagaManagerProxy<S> {
    pub(crate) fn new(inner: ProcessProxy<SagaManagerProcess<S>>) -> Self {
        Self { inner }
    }

    /// Returns the runtime pid of the underlying manager process.
    pub fn pid(&self) -> Pid {
        self.inner.pid()
    }

    /// Stops the manager and, with it, every source partition and saga instance
    /// it holds; each partition's offset stays where it was, so the next
    /// manager resumes from there. The delivery host and the deadline index are
    /// not stopped: they are top-level processes, and a recorded decision is
    /// still owed to its target.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`](nitinol_runtime::error::SendError) if the manager
    /// has already stopped.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}

/// The handle one hosted saga declaration hands back: the manager, the
/// delivery host that carries out what it decided, and, for a saga type that
/// declares a deadline, the deadline index.
///
/// - Dropping this stops nothing. Only the manager has a stop
///   ([`SagaManagerProxy::stop`]); the delivery host and the index keep running.
/// - A clone reaches the same processes, so an offset moved through one clone's
///   `delivery()` is the one the others read.
pub struct SagaHostProxy<S: Saga> {
    manager: SagaManagerProxy<S>,
    delivery: SagaDeliveryProxy<S>,
    deadline_index: Option<SagaDeadlineIndexProxy>,
}

impl<S: Saga> Clone for SagaHostProxy<S> {
    fn clone(&self) -> Self {
        Self {
            manager: self.manager.clone(),
            delivery: self.delivery.clone(),
            deadline_index: self.deadline_index.clone(),
        }
    }
}

impl<S: Saga> SagaHostProxy<S> {
    pub(crate) fn new(
        manager: SagaManagerProxy<S>,
        delivery: SagaDeliveryProxy<S>,
        deadline_index: Option<SagaDeadlineIndexProxy>,
    ) -> Self {
        Self {
            manager,
            delivery,
            deadline_index,
        }
    }

    /// The manager half: the instances, their source partitions, and the pid
    /// the whole fan-out is registered under.
    pub fn manager(&self) -> &SagaManagerProxy<S> {
        &self.manager
    }

    /// The delivery half, and with it the operator entries a running host is
    /// reached through —
    /// [`delivery_id`](SagaDeliveryProxy::delivery_id) and
    /// [`advance_offset`](SagaDeliveryProxy::advance_offset).
    pub fn delivery(&self) -> &SagaDeliveryProxy<S> {
        &self.delivery
    }

    /// The deadline index, when the saga type's alphabet declares a deadline,
    /// and `None` when it declares none: a saga type with no time of its own
    /// has no deadline to bring an instance back for, so none is started.
    pub fn deadline_index(&self) -> Option<&SagaDeadlineIndexProxy> {
        self.deadline_index.as_ref()
    }
}
