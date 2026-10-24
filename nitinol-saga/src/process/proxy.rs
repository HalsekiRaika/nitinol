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

    /// Stops the manager, cascade-stopping every source partition — one per
    /// declared source and partition — and every saga instance it currently
    /// holds. Each partition's recorded offset stays where its last turn put
    /// it, so the next manager over the same store resumes from there.
    ///
    /// The delivery host a hosted saga was started with is **not** stopped:
    /// it is a top-level process of the system rather than a child of the
    /// manager, and what it carries is already journalled. A decision that was
    /// recorded is owed to its target whether or not the instance that made it
    /// is still resident, so stopping the deciding half leaves the delivering
    /// half to finish.
    ///
    /// Nor is the deadline index, which is a top-level process for the same
    /// reason: it keeps asking this manager for the instances whose deadlines
    /// fall due, and reports that nothing answers.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}

/// The handle one hosted saga declaration hands back: the manager, the
/// delivery host that carries out what it decided, and — for a saga type that
/// declares a deadline — the deadline index that brings an instance back when
/// one of its deadlines falls due.
///
/// The saga writes its own stream; the delivery host and the index each read
/// it in global order, the delivery host from an offset it records and the
/// index from cursors it rebuilds whenever it starts. All of them are reached
/// through this rather than returned as a tuple, because one declaration
/// started them and there is no wiring in which a deployment holds one without
/// the others.
///
/// Dropping this stops nothing — the manager's fan-out, the host's partitions
/// and the index's partitions are processes of the system, exactly as each
/// part's own handle says.  Stopping is asked for part by part:
/// `manager().stop()` takes the deciding side down and leaves the delivering
/// side and the index running (stated on [`SagaManagerProxy::stop`]), and
/// neither a delivery host nor an index has a stop at all, which is the
/// contract [`SagaDeliveryProxy`] already carries.
///
/// Shared by cloning rather than by being wrapped: a clone reaches the same
/// manager process and the same running partitions, so `manager().stop()`
/// through any clone stops the one manager, and an offset moved through one
/// clone's `delivery()` is the offset the others read.
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
