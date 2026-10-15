//! The user-facing handles to a running saga: the one a hosted declaration
//! hands back, and the manager half it names.
//!
//! The runtime `ProcessProxy<_>` is wrapped so the user never sees the
//! `Process` trait or the internal process types.  The manager handle exposes
//! only `pid()` and `stop()` for ad-hoc control — sagas are reactive and are
//! driven by their subscriptions, not by direct calls from application code.

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;

use crate::delivery::SagaDeliveryProxy;
use crate::process::manager::SagaManagerProcess;
use crate::saga::Saga;

/// A typed handle to a running saga instance manager.
///
/// The manager's upstream pollers — one per declared source — and every
/// instance it spawned are runtime children of the manager process, so its
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

    /// Stops the manager, cascade-stopping every upstream poller — one per
    /// declared source — and every saga instance it currently holds.
    ///
    /// The delivery host a hosted saga was started with is **not** stopped:
    /// it is a top-level process of the system rather than a child of the
    /// manager, and what it carries is already journalled. A decision that was
    /// recorded is owed to its target whether or not the instance that made it
    /// is still resident, so stopping the deciding half leaves the delivering
    /// half to finish.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}

/// The handle one hosted saga declaration hands back: the manager, and the
/// delivery host that carries out what it decided.
///
/// Both halves are reached through this rather than returned as a pair,
/// because one declaration started them and there is no wiring in which a
/// deployment holds one without the other.
///
/// Dropping this stops nothing — the manager's fan-out and the host's
/// partitions are processes of the system, exactly as each half's own handle
/// says.  Stopping is asked for half by half: `manager().stop()` takes the
/// deciding side down and leaves the delivering side running (stated on
/// [`SagaManagerProxy::stop`]), and a delivery host has no stop at all, which
/// is the contract [`SagaDeliveryProxy`] already carries.
pub struct SagaHostProxy<S: Saga> {
    manager: SagaManagerProxy<S>,
    delivery: SagaDeliveryProxy<S>,
}

impl<S: Saga> SagaHostProxy<S> {
    pub(crate) fn new(manager: SagaManagerProxy<S>, delivery: SagaDeliveryProxy<S>) -> Self {
        Self { manager, delivery }
    }

    /// The manager half: the instances, their upstream pollers, and the pid
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
}
