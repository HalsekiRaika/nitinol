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
pub struct SagaProxy<S: Saga>(pub(crate) ProcessProxy<SagaProcess<S>>);

impl<S: Saga> Clone for SagaProxy<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S: Saga> SagaProxy<S> {
    /// Returns the runtime pid of the underlying saga process.
    pub fn pid(&self) -> Pid {
        self.0.pid()
    }
}
