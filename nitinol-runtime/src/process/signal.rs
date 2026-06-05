use crate::ident::Pid;
use crate::process::watch::TerminatedReason;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SystemSignal {
    Stop,
    Poison,
    /// Register `watcher_pid` as a watcher of this process.
    Watch {
        watcher_pid: Pid,
    },
    /// Remove `watcher_pid` from the watchers set of this process.
    Unwatch {
        watcher_pid: Pid,
    },
    /// Notify this process that a watched process has terminated.
    ///
    /// Sent to all explicit watchers of the terminating process (registered
    /// via `ctx.watch`), and also to the process's parent (if any) to enable
    /// parent/child lifecycle management regardless of explicit watch state.
    ///
    /// Whether `on_terminated` is invoked for the receiver depends on context:
    /// - For non-child Terminated: always invokes `on_terminated`.
    /// - For a child Terminated: invokes `on_terminated` only if the parent
    ///   explicitly called `ctx.watch` on that child.
    Terminated {
        who: Pid,
        why: TerminatedReason,
    },
}
