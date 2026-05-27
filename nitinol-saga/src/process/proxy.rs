//! User-facing handle to a running saga process.
//!
//! The runtime `ProcessProxy<SagaProcess<S>>` is wrapped so the user never
//! sees the `Process` trait or the internal `SagaProcess` type.  MVP exposes
//! only the saga's `pid()` because the MVP design forbids ad-hoc commands
//! into a saga from outside the subscription channel.

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::ProcessProxy;

use crate::process::saga_process::SagaProcess;
use crate::saga::Saga;

/// A typed handle to a running saga.
///
/// `Clone`-able like `AggregateProxy<A>`.  Per the MVP scope the proxy
/// exposes only the saga's pid — sagas are reactive and are driven by their
/// subscription, not by direct calls from application code.
///
/// The upstream `DurableStream` lifetime is owned by `SagaProcess` itself,
/// so dropping all copies of this handle does not affect the subscription.
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
    pub(crate) fn new(inner: ProcessProxy<SagaProcess<S>>) -> Self {
        Self { inner }
    }

    /// Returns the runtime pid of the underlying saga process.
    pub fn pid(&self) -> Pid {
        self.inner.pid()
    }

    /// Stops the underlying saga process.
    ///
    /// Stopping the process releases the `DurableStream` keepalive held inside
    /// `SagaProcess`, which in turn stops the upstream poller.  This is the
    /// correct way to terminate a saga; dropping all `SagaProxy` handles alone
    /// is not sufficient because the process is not owned by the handle.
    pub async fn stop(&self) -> Result<(), nitinol_runtime::error::SendError> {
        self.inner.stop().await
    }
}
