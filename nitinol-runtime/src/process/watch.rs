use crate::ident::Pid;

/// Notification delivered to a watcher when the watched process terminates.
#[derive(Debug, Clone)]
pub struct Terminated {
    pub who: Pid,
    pub why: TerminatedReason,
}

/// Reason for a `Terminated` notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminatedReason {
    /// The process stopped normally (or via Supervision `Stop` directive).
    Stopped,
    /// The watched process did not exist when `watch()` was called.
    NotFound,
}

/// Internal message routed through DeadLetter to trigger `Terminated{NotFound}`.
///
/// When `ctx.watch(pid)` finds that `pid` is absent from the registry, this
/// request is wrapped in a `DeadLetterEnvelope` and sent to `DeadLetterActor`.
/// The actor detects the type, looks up the watcher, and sends back
/// `SystemSignal::Terminated { why: NotFound }`.
pub(crate) struct WatchRequest {
    pub(crate) watched: Pid,
    pub(crate) watcher: Pid,
}
