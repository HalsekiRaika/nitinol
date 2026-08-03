//! Tests for Issue #60: `StashDriver` body — message stash queue + unstash.
//!
//! Issue #56 reserved the API surface (`ctx.stash` / `ctx.unstash_all`) as a
//! no-op stub. This issue lands the real queue / replay machinery. These tests
//! pin down the behaviors decided in `order.md` (lines 36-41):
//!
//! 1. A message stashed via `ctx.stash` is, after `unstash_all`, processed
//!    BEFORE newly arrived messages, in FIFO order.
//! 2. `unstash(n)` re-delivers exactly N messages; the remainder stays stashed.
//! 3. When the stash buffer is full, `ctx.stash` returns `Err(StashError::Full)`.
//! 4. When a full stash drops an `ask`-originated message, the caller receives
//!    `AskError::ReplyDropped`.
//! 5. `ctx.stash` / `ctx.unstash_all` are always operational (Core trio —
//!    cannot be opted out, even when a user driver is installed).
//!
//! ## Determinism
//!
//! The lifecycle loop is single-threaded per process and its `select!` is
//! `biased`. The Core trio places `StashDriver` first, so once `unstash*`
//! moves entries into the ready queue, they are fully drained BEFORE the
//! `MessageDriver` delivers the next mailbox message. Combined with FIFO
//! mailbox ordering, every assertion below is order-deterministic without
//! sleeps — an `ask` issued after the triggering `tell` observes a fully
//! settled log.

use std::convert::Infallible;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nitinol_runtime::error::{AskError, HandlerError, StashError};
use nitinol_runtime::process::{Driver, Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props, StashCapacity};

// ===========================================================================
// Ordering fixture (behaviors 1, 2, 5)
// ===========================================================================
//
// While `active == false` the process stashes every `Work`. Once `active`
// flips (via `UnstashAll` / `UnstashN`) the unstashed `Work`s replay through
// `recv` again and are appended to `log`, recording the processing order.

#[derive(Default)]
struct OrderProcess {
    active: bool,
    log: Arc<Mutex<Vec<u32>>>,
}

struct Work(u32);
struct UnstashAll;
struct UnstashN(usize);
struct GetLog;

impl Process for OrderProcess {}

impl Receive<Work> for OrderProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, msg: Work, ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        if self.active {
            self.log.lock().unwrap().push(msg.0);
        } else {
            ctx.stash(msg)
                .await
                .expect("stash within configured capacity must succeed");
        }
        Ok(())
    }
}

impl Receive<UnstashAll> for OrderProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        _: UnstashAll,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.active = true;
        ctx.unstash_all().await;
        Ok(())
    }
}

impl Receive<UnstashN> for OrderProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        msg: UnstashN,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.active = true;
        ctx.unstash(msg.0).await;
        Ok(())
    }
}

impl Receive<GetLog> for OrderProcess {
    type Response = Vec<u32>;
    type Error = Infallible;
    async fn recv(
        &mut self,
        _: GetLog,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Vec<u32>, Infallible> {
        Ok(self.log.lock().unwrap().clone())
    }
}

fn order_props() -> Props<OrderProcess> {
    Props::new(OrderProcess::default)
}

// ---------------------------------------------------------------------------
// Behavior 1: stashed messages replay BEFORE new arrivals, in FIFO order.
// ---------------------------------------------------------------------------

/// Given a process that stashes `Work(1)`, `Work(2)`, `Work(3)` while inactive,
/// when `UnstashAll` flips it active and `Work(4)` arrives afterward,
/// then the processing order is `[1, 2, 3, 4]` — the three stashed messages are
/// replayed (in stash order) ahead of the freshly arrived `Work(4)`.
#[tokio::test]
async fn unstash_all_replays_stashed_before_new_arrivals_in_fifo_order() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(order_props()).await;

    for n in [1, 2, 3] {
        proxy.tell(Work(n)).await.expect("tell Work must succeed");
    }
    proxy
        .tell(UnstashAll)
        .await
        .expect("tell UnstashAll must succeed");
    proxy
        .tell(Work(4))
        .await
        .expect("tell new Work must succeed");

    let log = proxy.ask(GetLog).await.expect("ask GetLog must succeed");
    assert_eq!(
        log,
        vec![1, 2, 3, 4],
        "stashed [1,2,3] must replay in FIFO order ahead of the new arrival 4"
    );
}

// ---------------------------------------------------------------------------
// Behavior 2: unstash(n) re-delivers exactly N; the remainder stays stashed.
// ---------------------------------------------------------------------------

/// Given five stashed `Work` messages,
/// when `unstash(2)` runs,
/// then exactly the first two replay and the other three remain stashed —
/// proven by a later `unstash_all` surfacing precisely `[3, 4, 5]` afterward.
#[tokio::test]
async fn unstash_n_redelivers_exactly_n_and_keeps_the_rest_stashed() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(order_props()).await;

    for n in [1, 2, 3, 4, 5] {
        proxy.tell(Work(n)).await.expect("tell Work must succeed");
    }

    proxy
        .tell(UnstashN(2))
        .await
        .expect("tell UnstashN(2) must succeed");
    let after_partial = proxy.ask(GetLog).await.expect("ask GetLog must succeed");
    assert_eq!(
        after_partial,
        vec![1, 2],
        "unstash(2) must replay exactly the first two stashed messages"
    );

    proxy
        .tell(UnstashAll)
        .await
        .expect("tell UnstashAll must succeed");
    let after_all = proxy.ask(GetLog).await.expect("ask GetLog must succeed");
    assert_eq!(
        after_all,
        vec![1, 2, 3, 4, 5],
        "the remaining [3,4,5] must have stayed stashed and replay in FIFO order"
    );
}

// ===========================================================================
// Capacity fixture (behavior 3)
// ===========================================================================
//
// Each `Probe` (tell) stashes a separate `Filler` and records whether the
// stash succeeded. `Filler` is never unstashed, so the `stashed` queue only
// grows — the capacity boundary is observed directly.

struct FullProcess {
    results: Arc<Mutex<Vec<bool>>>,
}

struct Probe;
struct Filler;
struct GetResults;

impl Process for FullProcess {}

impl Receive<Filler> for FullProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Filler, _ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        Ok(())
    }
}

impl Receive<Probe> for FullProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Probe, ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        let outcome = ctx.stash(Filler).await;
        let full = matches!(outcome, Err(StashError::Full));
        self.results.lock().unwrap().push(!full);
        Ok(())
    }
}

impl Receive<GetResults> for FullProcess {
    type Response = Vec<bool>;
    type Error = Infallible;
    async fn recv(
        &mut self,
        _: GetResults,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Vec<bool>, Infallible> {
        Ok(self.results.lock().unwrap().clone())
    }
}

/// Given a process configured with `StashCapacity::Bounded(2)`,
/// when three `Probe`s each stash a `Filler`,
/// then the first two succeed and the third returns `Err(StashError::Full)` —
/// the buffer accepts up to capacity and rejects the overflow without panic.
#[tokio::test]
async fn stash_returns_full_error_when_buffer_is_at_capacity() {
    let system = ProcessSystem::new().await;
    let results = Arc::new(Mutex::new(Vec::new()));
    let cap = StashCapacity::bounded(2).expect("bounded(2) must succeed");
    let results_for_proc = results.clone();
    let proxy = system
        .spawn(
            Props::new(move || FullProcess {
                results: results_for_proc.clone(),
            })
            .with_stash_capacity(cap),
        )
        .await;

    for _ in 0..3 {
        proxy.tell(Probe).await.expect("tell Probe must succeed");
    }

    let outcomes = proxy
        .ask(GetResults)
        .await
        .expect("ask GetResults must succeed");
    assert_eq!(
        outcomes,
        vec![true, true, false],
        "stash must accept up to capacity (2) then reject with StashError::Full"
    );
}

// ===========================================================================
// Drop fixture (behavior 4)
// ===========================================================================
//
// `Hold` is delivered via `ask` and stashed. A successful stash retains the
// reply channel (caller stays pending); a full-stash drop releases it, so the
// caller observes `AskError::ReplyDropped`.

struct DropProcess {
    received: Arc<AtomicU32>,
}

struct Hold;

impl Process for DropProcess {}

impl Receive<Hold> for DropProcess {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, msg: Hold, ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        self.received.fetch_add(1, Ordering::SeqCst);
        // On capacity overflow the stash must drop this ask's reply channel,
        // surfacing `AskError::ReplyDropped` at the caller.
        let _ = ctx.stash(msg).await;
        Ok(())
    }
}

async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for received counter to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Given a process with `StashCapacity::Bounded(1)` whose handler stashes the
/// in-flight `ask` message,
/// when a first `Hold` fills the buffer (its caller stays pending) and a second
/// `Hold` overflows it,
/// then the second caller receives `AskError::ReplyDropped` while the first
/// caller remains pending (its reply retained in the stash).
#[tokio::test]
async fn full_stash_drops_ask_reply_with_reply_dropped() {
    let system = ProcessSystem::new().await;
    let received = Arc::new(AtomicU32::new(0));
    let cap = StashCapacity::bounded(1).expect("bounded(1) must succeed");
    let received_for_proc = received.clone();
    let proxy = system
        .spawn(
            Props::new(move || DropProcess {
                received: received_for_proc.clone(),
            })
            .with_stash_capacity(cap),
        )
        .await;

    // First ask fills the single stash slot; its reply is retained, so the
    // caller stays pending. Run it off-task so we can issue the second ask.
    let first = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.ask(Hold).await })
    };

    // Once the first Hold has been received (and therefore stashed — recv runs
    // to completion before the next message), the buffer is full.
    wait_for_count(&received, 1).await;

    let second = proxy.ask(Hold).await;
    assert!(
        matches!(second, Err(AskError::ReplyDropped)),
        "overflowing ask must surface AskError::ReplyDropped, got {second:?}"
    );

    assert!(
        !first.is_finished(),
        "the first ask's reply must be retained in the stash (caller still pending)"
    );
    first.abort();
}

// ===========================================================================
// Always-operational fixture (behavior 5)
// ===========================================================================
//
// A user-supplied driver occupies the `with_driver` slot. The Core trio
// (including `StashDriver`) is still composed underneath, so stash/unstash
// must keep working — there is no way to opt out.

struct InertDriver;

impl Driver<OrderProcess> for InertDriver {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }

    async fn apply(
        &mut self,
        _state: &mut OrderProcess,
        _ctx: &mut ProcessContext<OrderProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
}

/// Given a process spawned WITH a user driver installed,
/// when it stashes `Work(1)`, `Work(2)` and later `unstash_all`s,
/// then both replay in FIFO order — proving the Core-trio `StashDriver` is
/// always mounted and cannot be opted out by supplying a user driver.
#[tokio::test]
async fn stash_is_always_operational_even_with_a_user_driver() {
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(order_props().with_driver(InertDriver)).await;

    proxy.tell(Work(1)).await.expect("tell Work(1) must succeed");
    proxy.tell(Work(2)).await.expect("tell Work(2) must succeed");
    proxy
        .tell(UnstashAll)
        .await
        .expect("tell UnstashAll must succeed");

    let log = proxy.ask(GetLog).await.expect("ask GetLog must succeed");
    assert_eq!(
        log,
        vec![1, 2],
        "stash/unstash must function end-to-end even when a user driver is installed"
    );
}
