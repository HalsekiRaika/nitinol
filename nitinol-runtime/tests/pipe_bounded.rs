//! Tests for bounded `PipeDriver` behavior at capacity exhaustion.
//!
//! `PipeDriver` previously used `tokio::sync::mpsc::unbounded_channel`
//! (`driver/pipe.rs:76`). It now uses a bounded channel whose capacity is
//! sourced from `Props::pipe_capacity` (or the `ProcessSystem` default for
//! `Inherit`).
//!
//! The runtime keeps the existing synchronous `pipe_to_self` API and uses
//! `try_send`:
//! - on-full and on-closed both drop silently;
//! - the actor stays alive (capacity exhaustion is NOT a crash).
//!
//! These tests pin down:
//! - A `Props::with_pipe_capacity(bounded(n))`-configured actor that pipes a
//!   burst far larger than `n` survives — the bounded channel rejects the
//!   surplus, but the lifecycle loop keeps running.
//! - At least one piped future is delivered (the bounded buffer of `n` slots
//!   does work; we do not regress to zero throughput).
//! - The actor still answers `ask` after the burst (proof of life).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{PipeCapacity, ProcessSystem, Props};

// Fixture: process whose handler enqueues N pipe futures in a tight loop, and
// counts mapped messages delivered back.

struct PipeBurstProcess {
    delivered: Arc<AtomicU32>,
}

struct BurstPipe {
    count: u32,
}

struct Delivered;

struct CountSnapshot;

struct Ping;

impl Process for PipeBurstProcess {}

impl Receive<BurstPipe> for PipeBurstProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        msg: BurstPipe,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // Each future sleeps before completing so the bounded buffer can fill
        // before any in-flight future drains a slot.
        for _ in 0..msg.count {
            ctx.pipe_to_self(
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                },
                |_| Delivered,
            );
        }
        Ok(())
    }
}

impl Receive<Delivered> for PipeBurstProcess {
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

impl Receive<CountSnapshot> for PipeBurstProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: CountSnapshot,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.delivered.load(Ordering::SeqCst))
    }
}

impl Receive<Ping> for PipeBurstProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Ping,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

fn burst_props(delivered: Arc<AtomicU32>) -> Props<PipeBurstProcess> {
    Props::new(move || PipeBurstProcess {
        delivered: delivered.clone(),
    })
}

// At-capacity drop semantics

/// Given a process configured with `PipeCapacity::Bounded(2)` and a handler
/// that bursts 64 pipe-future enqueues,
/// when the burst arrives faster than the bounded buffer can drain,
/// then:
///   1. the actor stays alive (subsequent `ask` succeeds);
///   2. fewer than 64 mapped messages arrive (proof that the bounded buffer
///      rejected the surplus rather than growing without bound).
#[tokio::test]
async fn pipe_bounded_drops_surplus_and_keeps_actor_alive() {
    let system = ProcessSystem::new().await;
    let delivered = Arc::new(AtomicU32::new(0));
    let cap = PipeCapacity::bounded(2).expect("bounded(2) must succeed");
    let proxy = system
        .spawn(burst_props(delivered.clone()).with_pipe_capacity(cap))
        .await;

    let attempted: u32 = 64;
    proxy
        .tell(BurstPipe { count: attempted })
        .await
        .expect("tell BurstPipe must succeed");

    // Allow the slow in-flight futures to complete and drain the buffer.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The actor is still alive (no panic from pipe_to_self over-subscription).
    proxy
        .ask(Ping)
        .await
        .expect("actor must remain alive after a pipe burst beyond capacity");

    let total = proxy
        .ask(CountSnapshot)
        .await
        .expect("ask CountSnapshot must succeed");

    assert!(
        total < attempted,
        "delivered ({total}) must be < enqueued ({attempted}); \
         the bounded buffer must drop surplus when full"
    );
}

/// Given a process configured with `PipeCapacity::Bounded(4)`,
/// when a burst of 4 pipe-future enqueues is followed by a brief wait,
/// then at least 1 mapped message is delivered — proving the bounded channel
/// is genuinely accepting up-to-capacity sends (i.e. the regression "bounded
/// drops EVERYTHING" would fail this test).
#[tokio::test]
async fn pipe_bounded_delivers_at_least_one_within_capacity() {
    let system = ProcessSystem::new().await;
    let delivered = Arc::new(AtomicU32::new(0));
    let cap = PipeCapacity::bounded(4).expect("bounded(4) must succeed");
    let proxy = system
        .spawn(burst_props(delivered.clone()).with_pipe_capacity(cap))
        .await;

    proxy
        .tell(BurstPipe { count: 4 })
        .await
        .expect("tell BurstPipe must succeed");

    // Wait long enough for the 50ms-each futures to complete and their
    // mapped messages to arrive.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = proxy
            .ask(CountSnapshot)
            .await
            .expect("ask CountSnapshot must succeed");
        if now >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no mapped message arrived within 5s — bounded buffer of 4 is rejecting \
             everything (regression from at-capacity drop to always-drop)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
