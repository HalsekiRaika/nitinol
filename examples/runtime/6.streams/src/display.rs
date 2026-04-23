//! Display subscriber for the streams example.
//!
//! `DisplaySubscriber` implements `Process + Receive<BoxedMessage>` directly —
//! the full handler API — rather than the simplified `Subscriber<T>` trait.
//! This lets it define `on_start` and `on_stop` lifecycle hooks, which
//! `Subscriber<T>` does not expose.
//!
//! Contrast with `AlertSubscriber`:
//! - `AlertSubscriber` → `Subscriber<BoxedMessage>` + `Props::subscriber`
//!   (no lifecycle hooks, simpler API, `()` return)
//! - `DisplaySubscriber` → `Process + Receive<BoxedMessage>` + `Props::new`
//!   (full lifecycle hooks, explicit `Result` return)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::BoxedMessage;
use tracing::info;

use crate::reading::TemperatureReading;

// ─── Process ─────────────────────────────────────────────────────────────────

/// Receives every published reading and logs it to stdout.
///
/// Demonstrates the `Process + Receive<BoxedMessage>` pattern.  Unlike
/// `Subscriber<BoxedMessage>`, the full `Receive` trait returns
/// `Result<(), Error>`, allowing the handler to propagate failures to the
/// supervision strategy.
pub struct DisplaySubscriber {
    received_count: Arc<AtomicU32>,
}

impl DisplaySubscriber {
    pub fn new(received_count: Arc<AtomicU32>) -> Self {
        Self { received_count }
    }
}

impl Process for DisplaySubscriber {
    async fn on_start(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        info!("[pid={pid}] DisplaySubscriber started");
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext) {
        let pid = ctx.pid();
        let total = self.received_count.load(Ordering::SeqCst);
        info!("[pid={pid}] DisplaySubscriber stopped (total received: {total})");
    }
}

// ─── Receive<BoxedMessage> ────────────────────────────────────────────────────

impl Receive<BoxedMessage> for DisplaySubscriber {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        self.received_count.fetch_add(1, Ordering::SeqCst);
        if let Some(reading) = msg.downcast_ref::<TemperatureReading>() {
            let sensor = &reading.sensor;
            let celsius = reading.celsius;
            info!("[DISPLAY] sensor={sensor} celsius={celsius:.1}");
        }
        Ok(())
    }
}
