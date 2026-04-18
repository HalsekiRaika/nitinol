mod any;

use std::any::Any;
use async_trait::async_trait;
pub use self::any::*;

use crate::ident::Pid;
use crate::process::signal::SystemSignal;
use crate::process::Process;
use tokio::sync::mpsc;
use crate::process::task::UserTask;

pub struct ProcessProxy<P> {
    pub(crate) pid: Pid,
    pub(crate) user_tx: mpsc::Sender<UserTask<P>>,
    pub(crate) sys_tx: mpsc::Sender<SystemSignal>,
}

impl<P> Clone for ProcessProxy<P> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            user_tx: self.user_tx.clone(),
            sys_tx: self.sys_tx.clone(),
        }
    }
}

#[async_trait]
impl<P> DynProxy for ProcessProxy<P>
where
    P: Process,
{
    fn as_any(&self) -> &dyn Any {
        self
    }
    
    async fn send_sys_sig(&self, signal: SystemSignal) {
        self.sys_tx.send(signal).await.unwrap()
    }
}