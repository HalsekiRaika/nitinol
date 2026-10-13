//! The user-facing handle to a running saga.
//!
//! The runtime `ProcessProxy<_>` is wrapped so the user never sees the
//! `Process` trait or the internal process types.  The handle exposes only
//! `pid()` and `stop()` for ad-hoc control — sagas are reactive and are driven
//! by their subscriptions, not by direct calls from application code.

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;

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
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}
