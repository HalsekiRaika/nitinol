//! Data transformer process for the supervision example.
//!
//! # Why Props?
//! The factory closure in `Props::new(DataTransformer::new)` lets the runtime
//! re-invoke it on each restart, producing a *brand-new* `DataTransformer` whose
//! `success_count` starts at zero.  If the instance were moved directly into a
//! Tokio task the runtime would have no way to recreate it after a failure.
//!
//! # Future note
//! After CQRS+ES integration the runtime will be able to replay events from an
//! event store to reconstruct pre-crash state, making `Restart` even more
//! powerful — restarts will no longer lose accumulated domain state.

use std::fmt;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use tracing::info;

// ─── Messages ────────────────────────────────────────────────────────────────

/// Request: parse the numeric string, double it, and return a formatted result.
///
/// Returns `Ok("n * 2 = result")` on success, or `Err(ParseError)` if the
/// string cannot be parsed as an integer — which triggers the supervision
/// strategy configured on `Props`.
pub struct Parse(pub String);

/// Query: return the number of successful `Parse` operations since the last
/// (re)start.  Demonstrates that `Restart` resets in-memory state to zero.
pub struct GetSuccessCount;

// ─── Error ───────────────────────────────────────────────────────────────────

/// Returned when the input string cannot be parsed as an integer.
#[derive(Debug)]
pub struct ParseError {
    pub input: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot parse {:?} as integer", self.input)
    }
}

impl std::error::Error for ParseError {}

// ─── Process ─────────────────────────────────────────────────────────────────

/// Stateful data transformer that counts how many valid parses it has handled.
///
/// Each time `Props` restarts this process, `DataTransformer::new` is called
/// again and `success_count` is reset to 0 — the factory closure creates a
/// fresh instance.
pub struct DataTransformer {
    success_count: u32,
}

impl DataTransformer {
    /// Factory function used as the `Props` producer closure.
    pub fn new() -> Self {
        Self { success_count: 0 }
    }
}

impl Process for DataTransformer {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        info!("[pid={pid}] DataTransformer started (success_count=0)");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        let count = self.success_count;
        info!("[pid={pid}] DataTransformer stopped (final success_count={count})");
    }
}

// ─── Receive<Parse> ──────────────────────────────────────────────────────────

impl Receive<Parse> for DataTransformer {
    type Response = String;
    type Error = ParseError;

    async fn recv(
        &mut self,
        msg: Parse,
        _ctx: &mut ProcessContext,
    ) -> Result<String, ParseError> {
        let n: i64 = msg.0.parse().map_err(|_| ParseError { input: msg.0.clone() })?;
        self.success_count += 1;
        Ok(format!("{} * 2 = {}", n, n * 2))
    }
}

// ─── Receive<GetSuccessCount> ────────────────────────────────────────────────

impl Receive<GetSuccessCount> for DataTransformer {
    type Response = u32;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: GetSuccessCount,
        _ctx: &mut ProcessContext,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.success_count)
    }
}
