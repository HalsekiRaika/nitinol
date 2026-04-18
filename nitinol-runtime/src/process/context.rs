use crate::ident::{Pid, ProcessName};

pub struct ProcessContext {
    pub(crate) pid: Pid,
    pub(crate) name: Option<ProcessName>,
}

impl ProcessContext {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name.as_ref()
    }
}
