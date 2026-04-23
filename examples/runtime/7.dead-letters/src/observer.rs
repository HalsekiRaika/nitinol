//! Dead-letter observer for the dead-letters example.
//!
//! Two subscriber types are provided:
//! - [`DeadLetterObserver`]: counts every `DeadLetter` envelope for polling assertions.
//! - [`DeadLetterInspector`]: performs the real two-stage downcast and prints
//!   `destination`, `sender`, and inner message type with actual runtime values.
//!
//! Both use the `Subscriber<BoxedMessage>` simplified API — no `Result` return,
//! no lifecycle hooks — identical to how `AlertSubscriber` is used in the
//! 6.streams example.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nitinol_runtime::process::ProcessContext;
use nitinol_runtime::{BoxedMessage, DeadLetter, Subscriber};

use crate::message::{Hush, Ping, Query};

// ─── Observer (counter) ───────────────────────────────────────────────────────

/// Counts every `DeadLetter` envelope delivered on the dead-letter stream.
///
/// The counter is incremented once per envelope regardless of the inner message
/// type, allowing tests and demos to assert how many dead letters were produced.
pub struct DeadLetterObserver {
    received: Arc<AtomicU32>,
}

impl DeadLetterObserver {
    pub fn new(received: Arc<AtomicU32>) -> Self {
        Self { received }
    }
}

impl Subscriber<BoxedMessage> for DeadLetterObserver {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        let received = self.received.clone();
        async move {
            // The outer message on the dead-letter stream is always BoxedMessage
            // wrapping a DeadLetter.  Skip anything that does not match.
            if msg.downcast_ref::<DeadLetter>().is_none() {
                return;
            }
            received.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ─── Inspector (field printer) ────────────────────────────────────────────────

/// Inspects each `DeadLetter` envelope and prints the real field values.
///
/// Performs the two-stage downcast (`BoxedMessage → DeadLetter → inner message`)
/// and prints `destination`, `sender`, and the resolved inner message type using
/// actual runtime values — not hardcoded strings.
pub struct DeadLetterInspector;

impl Subscriber<BoxedMessage> for DeadLetterInspector {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        async move {
            // Two-stage downcast: BoxedMessage → DeadLetter (outer)
            let Some(dl) = msg.downcast_ref::<DeadLetter>() else {
                return;
            };
            println!("  [inspector] destination: {}", dl.destination);
            match &dl.sender {
                Some(pid) => println!("  [inspector] sender: Some({pid})"),
                None => println!("  [inspector] sender: None (direct tell/ask)"),
            }
            // DeadLetter.message → inner type (inner)
            if dl.message.downcast_ref::<Ping>().is_some() {
                println!("  [inspector] inner message: Ping");
            } else if dl.message.downcast_ref::<Query>().is_some() {
                println!("  [inspector] inner message: Query");
            } else if dl.message.downcast_ref::<Hush>().is_some() {
                println!("  [inspector] inner message: Hush (SuppressDeadLetterLog)");
            } else {
                println!("  [inspector] inner message: (unknown type)");
            }
        }
    }
}
