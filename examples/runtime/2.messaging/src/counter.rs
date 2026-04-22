use std::convert::Infallible;
use std::future::Future;

use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::message::{Decrement, GetCount, Increment};

/// A counter process whose state lives inside a spawned tokio task.
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
        let pid = ctx.pid();
        let count = self.count;
        async move {
            println!("[pid={pid}] Counter stopped (final count={count})");
        }
    }
}

impl Receive<Increment> for Counter {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: Increment,
        _ctx: &mut ProcessContext,
    ) -> Result<(), Infallible> {
        self.count += 1;
        Ok(())
    }
}

impl Receive<Decrement> for Counter {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: Decrement,
        _ctx: &mut ProcessContext,
    ) -> Result<(), Infallible> {
        self.count = self.count.saturating_sub(1);
        Ok(())
    }
}

impl Receive<GetCount> for Counter {
    type Response = u32;
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: GetCount,
        _ctx: &mut ProcessContext,
    ) -> Result<u32, Infallible> {
        Ok(self.count)
    }
}
