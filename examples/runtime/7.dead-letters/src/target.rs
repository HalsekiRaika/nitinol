//! Target process for the dead-letters example.
//!
//! `TargetProcess` is intentionally stopped before messages are sent to it,
//! causing all messages to be routed to the dead-letter queue.  It handles
//! `Ping`, `Query`, and `Hush` to support all three demonstration scenarios.

use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::message::{Hush, Ping, Query};

// ─── Process ─────────────────────────────────────────────────────────────────

/// A minimal process that accepts `Ping`, `Query`, and `Hush`.
///
/// In normal operation its handlers simply return.  In the example it is
/// always stopped before messages arrive, so the handlers are never reached —
/// the runtime routes every incoming message to the dead-letter stream instead.
pub struct TargetProcess;

impl Process for TargetProcess {}

// ─── Receive<Ping> ───────────────────────────────────────────────────────────

impl Receive<Ping> for TargetProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(&mut self, _msg: Ping, _ctx: &mut ProcessContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ─── Receive<Query> ──────────────────────────────────────────────────────────

impl Receive<Query> for TargetProcess {
    type Response = u32;
    type Error = std::convert::Infallible;

    async fn recv(&mut self, _msg: Query, _ctx: &mut ProcessContext) -> Result<u32, Self::Error> {
        Ok(0)
    }
}

// ─── Receive<Hush> ───────────────────────────────────────────────────────────

impl Receive<Hush> for TargetProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(&mut self, _msg: Hush, _ctx: &mut ProcessContext) -> Result<(), Self::Error> {
        Ok(())
    }
}
