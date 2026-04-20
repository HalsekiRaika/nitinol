use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::SendError;
use crate::process::signal::SystemSignal;
use crate::process::{Process, ProcessProxy};

#[async_trait]
pub(crate) trait DynProxy: 'static + Sync + Send {
    fn as_any(&self) -> &dyn Any;
    async fn send_sys_sig(&self, signal: SystemSignal) -> Result<(), SendError>;
}

#[derive(Clone)]
pub struct AnyProxy(Arc<dyn DynProxy>);

impl AnyProxy {
    pub fn downcast<P: Process>(&self) -> Option<ProcessProxy<P>> {
        self.0.as_any().downcast_ref::<ProcessProxy<P>>().cloned()
    }

    pub async fn stop(&self) -> Result<(), SendError> {
        self.0.send_sys_sig(SystemSignal::Stop).await
    }

    pub(crate) async fn send_system_signal(&self, signal: SystemSignal) -> Result<(), SendError> {
        self.0.send_sys_sig(signal).await
    }
}

impl<P: Process> From<ProcessProxy<P>> for AnyProxy {
    fn from(proxy: ProcessProxy<P>) -> Self {
        Self(Arc::new(proxy))
    }
}
