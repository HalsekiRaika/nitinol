use std::future::Future;

use nitinol_runtime::process::{Process, ProcessContext};

/// A simple counter that demonstrates the Process lifecycle.
///
/// The count value lives entirely inside the tokio task that owns this process —
/// the spawning thread can only interact with it through a `ProcessProxy`.
pub struct Counter {
    count: u32,
}

impl Counter {
    pub fn new(initial: u32) -> Self {
        Self { count: initial }
    }
}

impl Process for Counter {
    fn on_start(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        // Capture by value: `ctx` is dropped here, before the future is returned.
        // This keeps the returned future `Send` without holding a `&mut ProcessContext` across await.
        let pid = ctx.pid();
        let count = self.count;
        async move {
            println!("[pid={pid}] Counter started (initial count={count})");
        }
    }

    fn on_stop(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        // Symmetric capture: ownership of `ctx` ends before the async block runs.
        let pid = ctx.pid();
        let count = self.count;
        async move {
            println!("[pid={pid}] Counter stopped (final count={count})");
        }
    }
}
