//! User-facing handles to the running saga processes.
//!
//! The runtime `ProcessProxy<_>` is wrapped so the user never sees the
//! `Process` trait or the internal process types.  Both handles expose only
//! `pid()` and `stop()` for ad-hoc control — sagas are reactive and are driven
//! by their subscription, not by direct calls from application code.

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;

use crate::process::manager::SagaManagerProcess;
use crate::process::saga_process::SagaProcess;
use crate::saga::Saga;

/// A typed handle to a running saga.
///
/// `Clone`-able like `AggregateProxy<A>`.  Per the MVP scope the proxy
/// exposes only the saga's pid — sagas are reactive and are driven by their
/// subscription, not by direct calls from application code.
///
/// The upstream `DirectPollerProcess` is spawned during the saga's `on_start`
/// as a runtime **child** of the saga process, so its lifetime is bound to
/// the saga's. Dropping every copy of this handle does not affect
/// the subscription — only stopping the saga (e.g. via [`Self::stop`] or
/// supervision) cascade-stops the upstream poller.
pub struct SagaProxy<S: Saga> {
    inner: ProcessProxy<SagaProcess<S>>,
}

impl<S: Saga> Clone for SagaProxy<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Saga> SagaProxy<S> {
    /// Returns the runtime pid of the underlying saga process.
    pub fn pid(&self) -> Pid {
        self.inner.pid()
    }

    /// Stops the underlying saga process.
    ///
    /// Stopping the process cascade-stops the upstream catchup poller, which
    /// was spawned as a runtime child during the saga's `on_start`.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}

impl<S: Saga> From<ProcessProxy<SagaProcess<S>>> for SagaProxy<S> {
    fn from(inner: ProcessProxy<SagaProcess<S>>) -> Self {
        Self { inner }
    }
}

/// A typed handle to a running saga instance manager.
///
/// The manager's upstream poller and every instance it spawned are runtime
/// children of the manager process, so its lifetime bounds the whole fan-out.
/// Dropping every copy of this handle does not affect them — only stopping the
/// manager (e.g. via [`Self::stop`] or supervision) does.
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
    /// Returns the runtime pid of the underlying manager process.
    pub fn pid(&self) -> Pid {
        self.inner.pid()
    }

    /// Stops the manager, cascade-stopping its upstream poller and every saga
    /// instance it currently holds.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}

impl<S: Saga> From<ProcessProxy<SagaManagerProcess<S>>> for SagaManagerProxy<S> {
    fn from(inner: ProcessProxy<SagaManagerProcess<S>>) -> Self {
        Self { inner }
    }
}
