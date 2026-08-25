//! Tests for `ProcessSystem` default-capacity templates and
//! `*Capacity::Inherit` decay at the spawn boundary.
//!
//! Previously, `ProcessSystem` carried only `default_idle_timeout` and the
//! mailbox / pipe buffers were hardcoded. Three more axes are now
//! configurable:
//!
//! ```text
//! ProcessSystem::builder()
//!     .with_default_mailbox_capacity(MailboxCapacity::bounded(64)?)
//!     .with_default_stash_capacity(StashCapacity::Bounded(NonZeroUsize::new(64).unwrap()))
//!     .with_default_pipe_capacity(PipeCapacity::Bounded(NonZeroUsize::new(64).unwrap()))
//!     .with_default_idle_timeout(Duration::from_secs(60))
//!     .build()
//!     .await;
//! ```
//!
//! The defaults are fixed on the builder and settled by `build()`: a built
//! `ProcessSystem` carries them and offers no way to change them.
//!
//! Under the "Inherit" decay rule, a `Props` field set to `Inherit` reads the
//! System default at spawn time; any explicit `Bounded(n)` on the `Props`
//! overrides the System default.
//!
//! These tests pin down:
//! - All three `with_default_*_capacity` setters consume `self` and return the
//!   builder, allowing a single fluent expression from `ProcessSystem::builder()`
//!   through to `build()`.
//! - Setting a system mailbox default of `bounded(1)` causes a `Props with
//!   mailbox=Inherit` to inherit that 1-slot bound — observed via `tell`
//!   blocking when the mailbox is full.
//! - Setting a system pipe default of `bounded(2)` causes a `Props with
//!   pipe=Inherit` to inherit that 2-slot bound — observed via pipe
//!   over-subscription dropping the surplus, while the actor stays alive.
//! - An explicit `Props::with_mailbox_capacity(Bounded(n))` overrides the
//!   System default (Inherit resolution at the Props level).
//!
//! Stash decay is type-level only here — the body lives in a separate issue
//! and the API surface is the part this issue must lock in.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{
    MailboxCapacity, PipeCapacity, ProcessSystem, ProcessSystemBuilder, Props, StashCapacity,
};

// Type-level: each `with_default_*_capacity` is a consuming builder method, and
// it lives on the builder rather than on a built system.
//
// If any of these regresses to `&mut Self`, the rebinding to
// `ProcessSystemBuilder` below stops compiling; if any moved back onto
// `ProcessSystem`, the receiver would no longer resolve.

#[allow(dead_code)]
fn _with_default_mailbox_capacity_is_consuming_builder() {
    let builder = ProcessSystem::builder();
    let _: ProcessSystemBuilder = builder.with_default_mailbox_capacity(MailboxCapacity::Inherit);
}

#[allow(dead_code)]
fn _with_default_stash_capacity_is_consuming_builder() {
    let builder = ProcessSystem::builder();
    let _: ProcessSystemBuilder = builder.with_default_stash_capacity(StashCapacity::Inherit);
}

#[allow(dead_code)]
fn _with_default_pipe_capacity_is_consuming_builder() {
    let builder = ProcessSystem::builder();
    let _: ProcessSystemBuilder = builder.with_default_pipe_capacity(PipeCapacity::Inherit);
}

/// All four default-axis setters chain into a single expression, which
/// `build()` closes by producing the system they configured.
#[allow(dead_code)]
async fn _full_system_default_chain_compiles_as_single_expression() {
    let n = NonZeroUsize::new(64).expect("64 is non-zero");
    let _: ProcessSystem = ProcessSystem::builder()
        .with_default_mailbox_capacity(MailboxCapacity::Bounded(n))
        .with_default_stash_capacity(StashCapacity::Bounded(n))
        .with_default_pipe_capacity(PipeCapacity::Bounded(n))
        .with_default_idle_timeout(Duration::from_secs(60))
        .build()
        .await;
}

// Mailbox Inherit decay — observed via send back-pressure.
//
// With a 1-slot mailbox and a handler that holds the slot indefinitely:
//   - tell #1: handler starts processing, slot becomes free.
//   - tell #2: queued in the 1-slot buffer (full).
//   - tell #3: send().await blocks waiting for a slot.
//
// A bounded `tokio::sync::mpsc::channel(1)` exposes this exactly.

/// Process whose handler awaits an external oneshot, holding the lifecycle
/// loop's mailbox-drain step until the test releases it. This is the simplest
/// way to keep a known number of messages queued.
struct BlockingProcess {
    block: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

struct Block;

impl Process for BlockingProcess {}

impl Receive<Block> for BlockingProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Block,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let rx = self.block.lock().await.take();
        if let Some(rx) = rx {
            // Holding here keeps the mailbox-drain step stuck until the test
            // sends on the corresponding sender.
            let _ = rx.await;
        }
        Ok(())
    }
}

fn blocking_props(rx: oneshot::Receiver<()>) -> Props<BlockingProcess> {
    let slot = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    Props::new(move || BlockingProcess {
        block: slot.clone(),
    })
}

/// Given a System with `MailboxCapacity::Bounded(1)` as its mailbox default,
/// and a `Props` whose mailbox is `Inherit` (the default),
/// when a blocking handler is holding the in-flight message and the test
/// sends two more messages without awaiting them,
/// then the third `tell` cannot complete within a short wall-clock window —
/// the bounded 1-slot mailbox inherited from the System default is exerting
/// back-pressure.
#[tokio::test]
async fn mailbox_inherit_decays_to_system_default_bounded_capacity() {
    let cap = MailboxCapacity::bounded(1).expect("bounded(1) must succeed");
    let system = ProcessSystem::builder()
        .with_default_mailbox_capacity(cap)
        .build()
        .await;

    let (release_tx, release_rx) = oneshot::channel();
    let proxy = system.spawn(blocking_props(release_rx)).await;

    // First message is taken into the handler (handler now awaits release_rx).
    proxy
        .tell(Block)
        .await
        .expect("first tell must succeed (handler picks it up)");

    // Brief settle so the lifecycle loop has moved msg#1 into the handler and
    // the 1-slot buffer is empty again.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Second message fills the single slot.
    proxy
        .tell(Block)
        .await
        .expect("second tell must succeed (fills the 1-slot buffer)");

    // Third message: with capacity 1 the buffer is now full and the handler
    // is still blocked, so tell must wait. Time-bound it.
    let blocked = tokio::time::timeout(Duration::from_millis(100), proxy.tell(Block)).await;
    assert!(
        blocked.is_err(),
        "third tell must NOT complete while the 1-slot mailbox is full; \
         bounded-1 inheritance is not in effect: {blocked:?}"
    );

    // Release the handler so the process can drain and shut down cleanly.
    let _ = release_tx.send(());
    proxy.stop().await.expect("stop must succeed");
}

/// Given a System with `MailboxCapacity::Bounded(1)`,
/// and a `Props` with `MailboxCapacity::Bounded(8)` (explicit override),
/// when the test sends three messages while the handler is blocked,
/// then all three `tell`s complete promptly — the explicit Props value
/// overrode the System default per the Inherit rule.
#[tokio::test]
async fn mailbox_explicit_bounded_overrides_system_default() {
    let system_cap = MailboxCapacity::bounded(1).expect("bounded(1) must succeed");
    let system = ProcessSystem::builder()
        .with_default_mailbox_capacity(system_cap)
        .build()
        .await;

    let (release_tx, release_rx) = oneshot::channel();
    let larger = MailboxCapacity::bounded(8).expect("bounded(8) must succeed");
    let proxy = system
        .spawn(blocking_props(release_rx).with_mailbox_capacity(larger))
        .await;

    // First tell drains into the handler.
    proxy
        .tell(Block)
        .await
        .expect("first tell must succeed (handler picks it up)");
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Two more tells must complete promptly because the Props override
    // gave us an 8-slot buffer, not the 1-slot system default.
    let two_more = tokio::time::timeout(Duration::from_millis(200), async {
        proxy
            .tell(Block)
            .await
            .expect("tell must succeed: the 8-slot mailbox override has free capacity");
        proxy
            .tell(Block)
            .await
            .expect("tell must succeed: the 8-slot mailbox override has free capacity");
    })
    .await;
    assert!(
        two_more.is_ok(),
        "with explicit MailboxCapacity::Bounded(8) on Props, two extra tells \
         must complete promptly; the system default of 1 must not apply"
    );

    let _ = release_tx.send(());
    proxy.stop().await.expect("stop must succeed");
}

// Pipe Inherit decay — the process whose `pipe_capacity=Inherit` reads the
// System default. We observe by piping into a slow handler from a tight loop
// and counting how many mapped messages arrive.
//
// For `pipe_to_self`, on-full and on-closed both drop silently. The actor
// stays alive.

#[derive(Default)]
struct PipeCountProcess {
    delivered: Arc<AtomicU32>,
}

struct PipeMany {
    /// How many futures to enqueue. Each one resolves to `Delivered`.
    count: u32,
}

struct Delivered;

impl Process for PipeCountProcess {}

impl Receive<PipeMany> for PipeCountProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        msg: PipeMany,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Each pipe future sleeps briefly so they all stay in-flight when the
        // sequence is registered — that lets the bounded send buffer fill.
        for _ in 0..msg.count {
            ctx.pipe_to_self(
                async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                },
                |_| Delivered,
            );
        }
        Ok(())
    }
}

impl Receive<Delivered> for PipeCountProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Delivered,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Snapshot;

impl Receive<Snapshot> for PipeCountProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Snapshot,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.delivered.load(Ordering::SeqCst))
    }
}

fn pipe_count_props(delivered: Arc<AtomicU32>) -> Props<PipeCountProcess> {
    Props::new(move || PipeCountProcess {
        delivered: delivered.clone(),
    })
}

/// Given a System with `PipeCapacity::Bounded(2)` and a `Props` whose
/// `pipe_capacity=Inherit`,
/// when a handler enqueues many pipe futures faster than the bounded buffer
/// can absorb them,
/// then the actor stays alive and the delivered count is BOUNDED (it does
/// NOT grow unboundedly with the number of enqueued futures). This pins down
/// both:
///   1. `pipe_capacity=Inherit` reads the System default, AND
///   2. `pipe_to_self` drops surplus enqueues when the bounded buffer is
///      full (silent drop by contract, not panic).
#[tokio::test]
async fn pipe_inherit_decays_to_system_default_and_drops_at_capacity() {
    let cap = PipeCapacity::bounded(2).expect("bounded(2) must succeed");
    let system = ProcessSystem::builder()
        .with_default_pipe_capacity(cap)
        .build()
        .await;

    let delivered = Arc::new(AtomicU32::new(0));
    let proxy = system.spawn(pipe_count_props(delivered.clone())).await;

    // Enqueue a number well above the bounded buffer in a single handler call.
    // The handler runs to completion before any pipe future is polled — so
    // every enqueue contends for the bounded send buffer up-front.
    let attempted: u32 = 32;
    proxy
        .tell(PipeMany { count: attempted })
        .await
        .expect("tell PipeMany must succeed");

    // Give the bounded buffer time to drain.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Actor must still be alive after the bounded buffer rejected the surplus.
    let final_count = proxy
        .ask(Snapshot)
        .await
        .expect("actor must still answer ask after pipe over-subscription");

    assert!(
        final_count <= attempted,
        "delivered ({final_count}) must not exceed enqueued ({attempted})"
    );
    assert!(
        final_count < attempted,
        "delivered ({final_count}) must be STRICTLY less than enqueued \
         ({attempted}) because the bounded buffer drops surplus; \
         otherwise pipe_capacity=Inherit did not pick up the system bound"
    );
}
