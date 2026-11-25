//! Integration tests for `ProcessWaker`, `Driver::attach_waker` and
//! `ProcessContext::waker`.
//!
//! These tests pin down:
//! - a handler that calls `ctx.waker().wake()` makes its own tick driver —
//!   one that honours `attach_waker` — deliver its next tick to `apply` at
//!   once, without waiting out the interval (so the lifecycle attaches the
//!   waker through `FusedDriver`, and `ctx.waker()` is that same waker)
//! - the lifecycle attaches the waker to the user driver exactly once
//! - a `wake` while no one waits is kept for the next `woken`, and repeated
//!   wakes fold into one tick
//! - a clone of the waker shares its permit: waking a clone from outside the
//!   process interrupts the driver while it sleeps
//! - a driver that does not implement `attach_waker` keeps its own behaviour
//!   when `combine_drivers!` puts it next to one that does, and `Combine`
//!   hands the waker to its right child too
//!
//! Every test runs on paused time, so "without waiting the interval" is read
//! off the virtual clock. The tick interval is an hour; every wait for a tick
//! is bounded far below it, so a tick that only arrives because the interval
//! ran out fails the test instead of passing it.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use nitinol_runtime::combine_drivers;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext, ProcessWaker, Receive};
use nitinol_runtime::{ProcessSystem, Props};

/// The tick period of every test driver here. Long enough that no wait for a
/// wake-up tick can be mistaken for an interval tick.
const INTERVAL: Duration = Duration::from_secs(3600);

/// How much virtual time a wake-up tick may take to reach `apply`.
const WAKE_LIMIT: Duration = Duration::from_secs(1);

/// How much virtual time to let pass when checking that no further tick came.
const QUIET: Duration = Duration::from_secs(60);

const SLOT_LOCK: &str = "the waker slot guards only a store or a take, so no holder panics";

// Test process. Its handlers reach the process's own waker through `ctx`.

struct Reader {
    shared_waker: Arc<Mutex<Option<ProcessWaker>>>,
}

impl Process for Reader {}

/// Asks the process to read now: the handler wakes its own driver once.
struct ReadNow;

/// Asks the process to read now, but the handler wakes its driver several
/// times in a row.
struct ReadNowRepeatedly(u32);

/// Hands a clone of the process's waker to the test, without waking it.
struct ShareWaker;

impl Receive<ReadNow> for Reader {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        _msg: ReadNow,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        ctx.waker().wake();
        Ok(())
    }
}

impl Receive<ReadNowRepeatedly> for Reader {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        msg: ReadNowRepeatedly,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        for _ in 0..msg.0 {
            ctx.waker().wake();
        }
        Ok(())
    }
}

impl Receive<ShareWaker> for Reader {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        _msg: ShareWaker,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        *self.shared_waker.lock().expect(SLOT_LOCK) = Some(ctx.waker().clone());
        Ok(())
    }
}

fn reader_props(shared_waker: Arc<Mutex<Option<ProcessWaker>>>) -> Props<Reader> {
    Props::new(move || Reader {
        shared_waker: shared_waker.clone(),
    })
}

// Test drivers. Both tick once per `INTERVAL`, the first tick one interval
// after the driver is first polled, so a fresh process has seen no tick.
// The deadline is kept across `next` calls: the lifecycle drops an unfinished
// `next` future whenever a message is handled, and that must not restart the
// period.

/// A tick driver that honours `attach_waker`: it waits for the deadline or a
/// wake, whichever comes first, and either way delivers one tick.
struct WakeableTicker {
    deadline: Option<Instant>,
    waker: Option<ProcessWaker>,
    ticks: Arc<AtomicU32>,
    attached: Arc<AtomicU32>,
}

impl WakeableTicker {
    fn new(ticks: Arc<AtomicU32>, attached: Arc<AtomicU32>) -> Self {
        Self {
            deadline: None,
            waker: None,
            ticks,
            attached,
        }
    }
}

impl Driver<Reader> for WakeableTicker {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + INTERVAL);
        match &self.waker {
            Some(waker) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {}
                    _ = waker.woken() => {}
                }
            }
            None => tokio::time::sleep_until(deadline).await,
        }
        self.deadline = Some(Instant::now() + INTERVAL);
        Some(())
    }

    async fn apply(
        &mut self,
        _state: &mut Reader,
        _ctx: &mut ProcessContext<Reader>,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }

    fn attach_waker(&mut self, waker: ProcessWaker) {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.waker = Some(waker);
    }
}

/// The same tick driver without `attach_waker`: it relies on the trait's
/// default and can only tick when its interval runs out.
struct PlainTicker {
    deadline: Option<Instant>,
    ticks: Arc<AtomicU32>,
}

impl PlainTicker {
    fn new(ticks: Arc<AtomicU32>) -> Self {
        Self {
            deadline: None,
            ticks,
        }
    }
}

impl Driver<Reader> for PlainTicker {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + INTERVAL);
        tokio::time::sleep_until(deadline).await;
        self.deadline = Some(Instant::now() + INTERVAL);
        Some(())
    }

    async fn apply(
        &mut self,
        _state: &mut Reader,
        _ctx: &mut ProcessContext<Reader>,
        _ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

// Helpers

fn counter() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0))
}

fn empty_slot() -> Arc<Mutex<Option<ProcessWaker>>> {
    Arc::new(Mutex::new(None))
}

/// Waits on the virtual clock until `ticks` reaches `expected`, failing once
/// `limit` has passed. Steps in 1 ms sleeps: with paused time the clock only
/// jumps to the earliest timer, which is this sleep as long as it is shorter
/// than every driver deadline, so the wait never skips past an interval.
async fn wait_for_ticks(ticks: &AtomicU32, expected: u32, limit: Duration, what: &str) {
    let deadline = Instant::now() + limit;
    while ticks.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "{what}: expected {expected} tick(s) within {limit:?} of virtual time, observed {}",
            ticks.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Lets `QUIET` of virtual time pass in 1 ms steps, so every task gets to run
/// in between.
async fn let_quiet_time_pass() {
    let until = Instant::now() + QUIET;
    while Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn take_shared_waker(slot: &Mutex<Option<ProcessWaker>>) -> Option<ProcessWaker> {
    slot.lock().expect(SLOT_LOCK).take()
}

// A handler's wake reaches its own driver.

/// Given a process whose user driver is a wakeable tick driver with an hour
/// interval,
/// when a handler calls `ctx.waker().wake()`,
/// then the next tick reaches `apply` well within the interval, and the
/// lifecycle attached the waker to the driver exactly once.
///
/// A tick that reaches `apply` early proves the waker crossed `FusedDriver`
/// into the user driver and that `ctx.waker()` is the attached waker, not a
/// separate one.
#[tokio::test(start_paused = true)]
async fn handler_wake_delivers_next_tick_without_waiting_the_interval() {
    let system = ProcessSystem::new().await;
    let ticks = counter();
    let attached = counter();
    let driver = WakeableTicker::new(ticks.clone(), attached.clone());
    let proxy = system
        .spawn(reader_props(empty_slot()).with_driver(driver))
        .await;
    let started = Instant::now();

    proxy.tell(ReadNow).await.expect("tell ReadNow");
    wait_for_ticks(&ticks, 1, WAKE_LIMIT, "tick after ctx.waker().wake()").await;

    assert!(
        started.elapsed() < INTERVAL,
        "the woken tick must not wait out the interval; {:?} of virtual time passed",
        started.elapsed()
    );
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "one wake must give exactly one tick"
    );
    assert_eq!(
        attached.load(Ordering::SeqCst),
        1,
        "the lifecycle must attach the waker to the user driver exactly once"
    );
}

// The permit: kept while no one waits, folded into one.

/// Given a process whose user driver is a wakeable tick driver,
/// when a handler wakes the driver three times in a row,
/// then exactly one early tick follows, no more ticks come while the interval
/// has not run out, and the driver's own period still delivers the next tick.
///
/// While a handler runs, the lifecycle is not polling the driver's `next`, so
/// no one waits on the waker: the wakes must be kept for the next `woken`
/// (otherwise no early tick comes) and folded into one permit (otherwise three
/// ticks come).
#[tokio::test(start_paused = true)]
async fn wake_while_no_one_waits_is_kept_and_repeated_wakes_fold_into_one_tick() {
    let system = ProcessSystem::new().await;
    let ticks = counter();
    let attached = counter();
    let driver = WakeableTicker::new(ticks.clone(), attached.clone());
    let proxy = system
        .spawn(reader_props(empty_slot()).with_driver(driver))
        .await;

    proxy
        .tell(ReadNowRepeatedly(3))
        .await
        .expect("tell ReadNowRepeatedly");
    wait_for_ticks(&ticks, 1, WAKE_LIMIT, "tick after three wakes").await;

    let_quiet_time_pass().await;
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "three wakes with no one waiting must fold into a single tick"
    );

    tokio::time::sleep(INTERVAL).await;
    wait_for_ticks(&ticks, 2, WAKE_LIMIT, "tick once the interval ran out").await;
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        2,
        "after the folded wake, the driver must tick on its own period again"
    );
}

/// Given a process whose wakeable tick driver is asleep, and a clone of its
/// waker taken out of a handler,
/// when the test wakes that clone from outside the process,
/// then the sleeping driver is interrupted and ticks at once.
///
/// This is the case the handler tests cannot reach: the wake lands while the
/// driver is parked on `woken`, and it comes through a clone, so the clone
/// must share the permit with the attached waker.
#[tokio::test(start_paused = true)]
async fn waking_a_clone_interrupts_the_driver_while_it_sleeps() {
    let system = ProcessSystem::new().await;
    let ticks = counter();
    let attached = counter();
    let slot = empty_slot();
    let driver = WakeableTicker::new(ticks.clone(), attached.clone());
    let proxy = system
        .spawn(reader_props(slot.clone()).with_driver(driver))
        .await;

    proxy.tell(ShareWaker).await.expect("tell ShareWaker");
    let started = Instant::now();
    let waker = loop {
        if let Some(waker) = take_shared_waker(&slot) {
            break waker;
        }
        assert!(
            started.elapsed() < WAKE_LIMIT,
            "timed out waiting for the handler to share the waker"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    };

    let_quiet_time_pass().await;
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        0,
        "taking a clone of the waker must not wake the driver"
    );

    let woke_at = Instant::now();
    waker.wake();
    wait_for_ticks(&ticks, 1, WAKE_LIMIT, "tick after waking the clone").await;

    assert!(
        woke_at.elapsed() < INTERVAL,
        "the sleeping driver must be interrupted, not wait out the interval"
    );
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "one wake must give exactly one tick"
    );
}

// A driver without `attach_waker` keeps its behaviour.

/// Given `combine_drivers!(plain, wakeable)` installed via `with_driver`, both
/// with an hour interval,
/// when a handler calls `ctx.waker().wake()`,
/// then the wakeable driver — the right child of the `Combine` — ticks at
/// once while the plain driver does not, and once the interval runs out the
/// plain driver still ticks on its own period.
#[tokio::test(start_paused = true)]
async fn driver_without_attach_waker_is_unaffected_when_combined_with_one_that_honours_it() {
    let system = ProcessSystem::new().await;
    let plain_ticks = counter();
    let wakeable_ticks = counter();
    let attached = counter();
    let driver = combine_drivers!(
        PlainTicker::new(plain_ticks.clone()),
        WakeableTicker::new(wakeable_ticks.clone(), attached.clone())
    );
    let proxy = system
        .spawn(reader_props(empty_slot()).with_driver(driver))
        .await;

    proxy.tell(ReadNow).await.expect("tell ReadNow");
    wait_for_ticks(
        &wakeable_ticks,
        1,
        WAKE_LIMIT,
        "wakeable tick after ctx.waker().wake()",
    )
    .await;

    let_quiet_time_pass().await;
    assert_eq!(
        wakeable_ticks.load(Ordering::SeqCst),
        1,
        "the wakeable driver must tick once for one wake"
    );
    assert_eq!(
        plain_ticks.load(Ordering::SeqCst),
        0,
        "a driver without attach_waker must not tick on a wake"
    );
    assert_eq!(
        attached.load(Ordering::SeqCst),
        1,
        "Combine must hand the waker to its right child exactly once"
    );

    tokio::time::sleep(INTERVAL).await;
    wait_for_ticks(
        &plain_ticks,
        1,
        WAKE_LIMIT,
        "plain tick once its interval ran out",
    )
    .await;
    assert_eq!(
        plain_ticks.load(Ordering::SeqCst),
        1,
        "the plain driver must keep ticking on its own period"
    );
}

// Type-level checks on the published surface.

fn assert_clone_send_sync<T: Clone + Send + Sync>() {}
fn assert_send<T: Send>(_: T) {}

/// `ProcessWaker` is `Clone + Send + Sync`, and the future `woken` returns is
/// `Send`, so it can be awaited inside a `Driver::next`.
///
/// Not a `#[test]` — the compiler enforces the bounds at build time.
#[allow(dead_code)]
fn _assert_process_waker_bounds(waker: &ProcessWaker) {
    assert_clone_send_sync::<ProcessWaker>();
    assert_send(waker.woken());
}

/// A handler reads its waker by reference from its context.
///
/// Not a `#[test]` — the compiler enforces the signature at build time.
#[allow(dead_code)]
fn _assert_context_waker_signature() {
    let _: for<'a> fn(&'a ProcessContext<Reader>) -> &'a ProcessWaker =
        ProcessContext::<Reader>::waker;
}
