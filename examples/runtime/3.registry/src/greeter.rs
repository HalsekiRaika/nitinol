use std::convert::Infallible;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use tracing::info;

/// A stateless greeter process.
///
/// Demonstrates that named processes can be any Process implementor — including
/// unit structs with no mutable state. Useful for showing that `AnyProxy::downcast`
/// type discrimination is based on the concrete Process type, not its fields.
pub struct Greeter;

impl Process for Greeter {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        info!("[pid={pid}] Greeter started");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        info!("[pid={pid}] Greeter stopped");
    }
}

/// Request-response query: returns a greeting string for the given name.
pub struct Greet(pub String);

impl Receive<Greet> for Greeter {
    type Response = String;
    type Error = Infallible;

    async fn recv(&mut self, msg: Greet, _ctx: &mut ProcessContext) -> Result<String, Infallible> {
        Ok(format!("Hello, {}!", msg.0))
    }
}
