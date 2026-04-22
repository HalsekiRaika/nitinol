use std::collections::VecDeque;
use std::convert::Infallible;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, Receive, Terminated, TerminatedReason};

/// Pipeline source stage: vends text items one-by-one and watches the downstream Transformer.
///
/// Demonstrates the upstream end of a watch chain. When the Transformer stops
/// (because its own downstream stopped), the Producer is notified via `on_terminated`.
pub struct Producer {
    /// Remaining items to vend, in FIFO order.
    items: VecDeque<String>,
    /// Pid of the downstream Transformer, used in `on_start` to register a watch.
    transformer_pid: Pid,
    /// Reason received from `on_terminated`, or `None` if no notification yet.
    downstream_terminated_reason: Option<TerminatedReason>,
}

impl Producer {
    pub fn new(items: Vec<String>, transformer_pid: Pid) -> Self {
        Self {
            items: items.into_iter().collect(),
            transformer_pid,
            downstream_terminated_reason: None,
        }
    }
}

impl Process for Producer {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!(
            "[pid={pid}] Producer started ({} items), watching transformer pid={}",
            self.items.len(),
            self.transformer_pid
        );
        ctx.watch(self.transformer_pid).await;
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!("[pid={pid}] Producer stopped");
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!(
            "[pid={pid}] Producer received Terminated {{ who={}, why={:?} }}",
            terminated.who, terminated.why
        );
        self.downstream_terminated_reason = Some(terminated.why);
    }
}

/// Request-response query: returns the next item in FIFO order, or `None` when exhausted.
pub struct Pull;

/// Request-response query: returns the `TerminatedReason` received by `on_terminated`,
/// or `None` if no notification has arrived yet.
pub struct CheckDownstream;

impl Receive<Pull> for Producer {
    type Response = Option<String>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: Pull,
        _ctx: &mut ProcessContext,
    ) -> Result<Option<String>, Infallible> {
        Ok(self.items.pop_front())
    }
}

impl Receive<CheckDownstream> for Producer {
    type Response = Option<TerminatedReason>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: CheckDownstream,
        _ctx: &mut ProcessContext,
    ) -> Result<Option<TerminatedReason>, Infallible> {
        Ok(self.downstream_terminated_reason)
    }
}
