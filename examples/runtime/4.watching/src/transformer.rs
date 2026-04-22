use std::fmt;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, Receive, Terminated, TerminatedReason};

/// Pipeline middle stage: converts text to uppercase and watches the downstream Collector.
///
/// Demonstrates `on_terminated` + cascade-stop: when the Collector stops, the
/// transformer marks its downstream as dead. The next `Transform` message then
/// returns `Err(DownstreamTerminated)`, which causes `SupervisionStrategy::Stop`
/// to fire and propagate the termination signal up to the Producer.
pub struct Transformer {
    /// Pid of the downstream Collector, used in `on_start` to register a watch.
    collector_pid: Pid,
    /// Reason received from `on_terminated`, or `None` if no notification yet.
    downstream_terminated_reason: Option<TerminatedReason>,
    /// Becomes `false` when `on_terminated` fires; gates the error path in `recv`.
    downstream_alive: bool,
}

impl Transformer {
    pub fn new(collector_pid: Pid) -> Self {
        Self {
            collector_pid,
            downstream_terminated_reason: None,
            downstream_alive: true,
        }
    }
}

impl Process for Transformer {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!("[pid={pid}] Transformer started, watching collector pid={}", self.collector_pid);
        ctx.watch(self.collector_pid).await;
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!("[pid={pid}] Transformer stopped");
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!(
            "[pid={pid}] Transformer received Terminated {{ who={}, why={:?} }}",
            terminated.who, terminated.why
        );
        self.downstream_terminated_reason = Some(terminated.why);
        self.downstream_alive = false;
    }
}

/// Error returned by `Transform` when the downstream Collector has already stopped.
///
/// Returning this error triggers `SupervisionStrategy::Stop`, which stops the
/// Transformer and notifies its own watchers (the Producer).
#[derive(Debug)]
pub struct DownstreamTerminated;

impl fmt::Display for DownstreamTerminated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "downstream collector has terminated")
    }
}

impl std::error::Error for DownstreamTerminated {}

/// Request-response command: transforms the input text to uppercase.
///
/// Returns `Err(DownstreamTerminated)` if the downstream Collector has stopped,
/// causing the lifecycle loop to apply the supervision strategy.
pub struct Transform(pub String);

/// Fire-and-forget command: unregisters the watch on the downstream Collector.
///
/// Useful for demonstrating that `ctx.unwatch` prevents future `Terminated`
/// notifications even when the watched process subsequently stops.
pub struct DetachDownstream;

/// Request-response query: returns the `TerminatedReason` received by `on_terminated`,
/// or `None` if no notification has arrived yet (or after `DetachDownstream`).
pub struct CheckDownstream;

impl Receive<Transform> for Transformer {
    type Response = String;
    type Error = DownstreamTerminated;

    async fn recv(
        &mut self,
        msg: Transform,
        _ctx: &mut ProcessContext,
    ) -> Result<String, DownstreamTerminated> {
        if !self.downstream_alive {
            return Err(DownstreamTerminated);
        }
        Ok(msg.0.to_uppercase())
    }
}

impl Receive<DetachDownstream> for Transformer {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: DetachDownstream,
        ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        ctx.unwatch(self.collector_pid).await;
        Ok(())
    }
}

impl Receive<CheckDownstream> for Transformer {
    type Response = Option<TerminatedReason>;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: CheckDownstream,
        _ctx: &mut ProcessContext,
    ) -> Result<Option<TerminatedReason>, std::convert::Infallible> {
        Ok(self.downstream_terminated_reason)
    }
}
