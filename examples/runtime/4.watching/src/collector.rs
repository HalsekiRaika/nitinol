use std::convert::Infallible;

use nitinol_runtime::process::{Process, ProcessContext, Receive};

/// Pipeline terminal stage: accumulates transformed text items in arrival order.
///
/// Demonstrates the simplest participant in a watch-based pipeline — the
/// downstream end that holds no watch references itself.
pub struct Collector {
    items: Vec<String>,
}

impl Collector {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }
}

impl Process for Collector {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!("[pid={pid}] Collector started");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        println!("[pid={pid}] Collector stopped ({} items collected)", self.items.len());
    }
}

/// Fire-and-forget command: appends one text item to the collection.
pub struct Collect(pub String);

/// Request-response query: returns all collected items in arrival order.
pub struct GetCollected;

impl Receive<Collect> for Collector {
    type Response = ();
    type Error = Infallible;

    async fn recv(&mut self, msg: Collect, _ctx: &mut ProcessContext) -> Result<(), Infallible> {
        self.items.push(msg.0);
        Ok(())
    }
}

impl Receive<GetCollected> for Collector {
    type Response = Vec<String>;
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: GetCollected,
        _ctx: &mut ProcessContext,
    ) -> Result<Vec<String>, Infallible> {
        Ok(self.items.clone())
    }
}
