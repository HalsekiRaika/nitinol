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

//noinspection DuplicatedCode
impl Process for Counter {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        let count = self.count;
        println!("[pid={pid}] Counter started (initial count={count})");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        let count = self.count;
        println!("[pid={pid}] Counter stopped (final count={count})");
    }
}
