use std::any::Any;
use std::sync::Arc;
use async_trait::async_trait;
use crate::process::{Process, ProcessProxy};
use crate::process::signal::SystemSignal;

#[async_trait]
pub trait DynProxy: 'static + Sync + Send {
    fn as_any(&self) -> &dyn Any;
    async fn send_sys_sig(&self, signal: SystemSignal);
}

pub struct AnyProxy(Arc<dyn DynProxy>);

impl AnyProxy {
    pub fn downcast<P: Process>(&self) -> Result<ProcessProxy<P>, ()> {
        self.0.as_any()
            .downcast_ref::<ProcessProxy<P>>()
            .cloned()
            .ok_or(())
    }
}

impl<P: Process> From<ProcessProxy<P>> for AnyProxy {
    fn from(proxy: ProcessProxy<P>) -> Self {
        Self(Arc::new(proxy))
    }
}