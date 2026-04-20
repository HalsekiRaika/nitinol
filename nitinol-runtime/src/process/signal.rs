use crate::ident::Pid;
use crate::process::watch::TerminatedReason;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SystemSignal {
    Stop,
    Poison,
    /// Register `watcher_pid` as a watcher of this process.
    Watch { watcher_pid: Pid },
    /// Remove `watcher_pid` from the watchers set of this process.
    Unwatch { watcher_pid: Pid },
    /// Notify this process that a watched process has terminated.
    Terminated { who: Pid, why: TerminatedReason },
}
