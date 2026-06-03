use std::convert::Infallible;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use tracing::info;

/// A counter process whose value is mutated via messages.
///
/// Demonstrates named process registration: this Counter can be found by name
/// or by Pid after spawning, and its typed proxy recovered via `AnyProxy::downcast`.
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

/// Fire-and-forget command: increments the counter by 1.
pub struct Increment;

/// Fire-and-forget command: decrements the counter by 1 (saturates at 0).
pub struct Decrement;

/// Request-response query: returns the current counter value.
pub struct GetCount;

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
