use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::ident::Pid;

/// Where a process is in its lifecycle, as read through
/// [`ProcessProxy::state`](crate::process::ProcessProxy::state).
///
/// A process moves through the states in order and never goes back. A
/// supervision restart is not a stop: the process stays `Running` through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The process has been spawned and its first `on_start` has not finished
    /// yet.
    Starting,
    /// `on_start` has finished and the process is handling messages.
    Running,
    /// The process has stopped handling messages and is shutting down: its
    /// `on_stop` runs (unless it was poisoned) and its children are stopped.
    /// Its mailbox is closed, so a `tell` or `ask` sent to it goes to the dead
    /// letters.
    Stopping,
    /// The process has finished shutting down and has left the registry.
    Stopped,
}

/// The one [`ProcessState`] a process and every clone of its proxy share.
#[derive(Clone)]
pub(crate) struct SharedProcessState(Arc<AtomicU8>);

impl SharedProcessState {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(ProcessState::Starting as u8)))
    }

    pub(crate) fn load(&self) -> ProcessState {
        const STARTING: u8 = ProcessState::Starting as u8;
        const RUNNING: u8 = ProcessState::Running as u8;
        const STOPPING: u8 = ProcessState::Stopping as u8;
        const STOPPED: u8 = ProcessState::Stopped as u8;
        match self.0.load(Ordering::Acquire) {
            STARTING => ProcessState::Starting,
            RUNNING => ProcessState::Running,
            STOPPING => ProcessState::Stopping,
            STOPPED => ProcessState::Stopped,
            other => unreachable!(
                "SharedProcessState only ever stores a ProcessState discriminant, got {other}"
            ),
        }
    }

    pub(crate) fn store(&self, state: ProcessState) {
        self.0.store(state as u8, Ordering::Release);
    }
}

/// Notification delivered to a watcher when the watched process terminates.
#[derive(Debug, Clone)]
pub struct Terminated {
    /// The pid of the process that terminated.
    pub who: Pid,
    /// Why the process terminated.
    pub why: TerminatedReason,
}

/// Reason for a `Terminated` notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminatedReason {
    /// The process stopped normally (via a Supervision `Stop` directive or channel close).
    Stopped,
    /// The watched process did not exist when `watch()` was called.
    NotFound,
    /// The process stopped because it was idle for longer than its configured timeout.
    Timeout,
    /// The process was force-terminated without cleanup (via `Poison` directive).
    Poisoned,
}

/// Internal message routed through DeadLetter to trigger `Terminated{NotFound}`.
///
/// When `ctx.watch(pid)` finds that `pid` is absent from the registry, this
/// request is wrapped in a `DeadLetterEnvelope` and sent to `DeadLetterProcess`.
/// The process detects the type, looks up the watcher, and sends back
/// `SystemSignal::Terminated { why: NotFound }`.
pub(crate) struct WatchRequest {
    pub(crate) watched: Pid,
    pub(crate) watcher: Pid,
}
