//! Alert subscriber for the streams example.
//!
//! `AlertSubscriber` uses the `Subscriber<T>` trait — the simplified handler
//! API — rather than implementing `Process + Receive<T>` directly.  The key
//! difference is that `Subscriber::recv` returns `()` instead of
//! `Result<(), E>`, removing the need for an explicit error type.
//!
//! Use `Props::subscriber(|| AlertSubscriber::new(...))` to spawn it.  The
//! runtime wraps it in an internal `SubscriberProcess` adapter that implements
//! the full `Receive<BoxedMessage>` trait behind the scenes.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nitinol_runtime::process::ProcessContext;
use nitinol_runtime::{BoxedMessage, Subscriber};
use tracing::info;

use crate::reading::TemperatureReading;

// ─── Process ─────────────────────────────────────────────────────────────────

/// Monitors published readings and counts events that exceed the threshold.
///
/// Demonstrates the `Subscriber<BoxedMessage>` simplified API: no `Result`
/// return type, no explicit error handling — handle the message and
/// return.  Contrast with `DisplaySubscriber`, which uses `Receive<BoxedMessage>`
/// and returns `Result<(), Infallible>`.
pub struct AlertSubscriber {
    threshold: f64,
    alert_count: Arc<AtomicU32>,
}

impl AlertSubscriber {
    pub fn new(threshold: f64, alert_count: Arc<AtomicU32>) -> Self {
        Self {
            threshold,
            alert_count,
        }
    }
}

// ─── Subscriber<BoxedMessage> ─────────────────────────────────────────────────

impl Subscriber<BoxedMessage> for AlertSubscriber {
    async fn recv(&mut self, msg: BoxedMessage, _ctx: &mut ProcessContext) {
        let Some(reading) = msg.downcast_ref::<TemperatureReading>() else {
            return;
        };
        if reading.celsius > self.threshold {
            self.alert_count.fetch_add(1, Ordering::SeqCst);
            let sensor = &reading.sensor;
            let celsius = reading.celsius;
            info!(
                "[ALERT] sensor={sensor} celsius={celsius:.1} exceeds threshold={threshold:.1}",
                threshold = self.threshold
            );
        }
    }
}
