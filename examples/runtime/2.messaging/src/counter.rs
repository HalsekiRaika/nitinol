use std::convert::Infallible;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use tracing::info;

use crate::message::{Decrement, GetCount, Increment};

/// A counter-process whose state lives inside a spawned tokio task.
///
/// The only way to read or modify `count` from outside the task is through
/// the `ProcessProxy` — Tell for writes, Ask for reads.
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
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let pid = ctx.pid();
        let count = self.count;
        info!("[pid={pid}] Counter started (initial count={count})");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext<Self>) {
        let pid = ctx.pid();
        let count = self.count;
        info!("[pid={pid}] Counter stopped (final count={count})");
    }
}

// noinspection DuplicatedCode
impl Receive<Increment> for Counter {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: Increment,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.count += 1;
        Ok(())
    }
}

// noinspection DuplicatedCode
impl Receive<Decrement> for Counter {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: Decrement,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.count = self.count.saturating_sub(1);
        Ok(())
    }
}

// noinspection DuplicatedCode
impl Receive<GetCount> for Counter {
    type Response = u32;
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: GetCount,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, Infallible> {
        Ok(self.count)
    }
}
