//! Tests for Issue #57: `Props<P, D = DefaultDriver>` — single-slot Driver
//! type parameter and the `with_driver` type-transitioning builder.
//!
//! Pre-spec, `Props::add_driver` pushed heterogeneous `Driver<P>` impls into a
//! `Vec<Box<dyn DynDriver<P>>>` via a runtime adapter (`DynAdapter`) and ran
//! them through a `select_all`-based `DynDriverSet`. Issue #57 lifts the
//! driver into the type parameter `D` of `Props`, makes the slot single, and
//! delegates multi-driver composition entirely to the static
//! [`combine_drivers!`](nitinol_runtime::combine_drivers) macro.
//!
//! These tests pin down the *new* contract that did not exist pre-spec and
//! that the migrated `combine_drivers.rs` / `spawn_with_driver.rs` /
//! `props_builder_chain.rs` / `unified_spawn.rs` / `pipe_to_self.rs` /
//! `child_processes.rs` test files do not by themselves enforce:
//!
//! 1. The default type parameter of `Props::new` is exactly `DefaultDriver`
//!    (type-level pin).
//! 2. `with_driver<D2>` is a **type-transitioning** builder:
//!    `Props<P, D> -> Props<P, D2>`. It does NOT keep `Self` (which would
//!    silently revive the pre-spec "append to a Vec" behavior).
//! 3. Calling `with_driver` twice REPLACES the slot, not appends — the second
//!    call's driver type is the only one that survives in the final type.
//! 4. After `with_driver`, the rest of the builder chain (`with_name`,
//!    `with_idle_timeout`, ...) still composes and preserves the driver type.
//! 5. A process spawned without any `with_driver` call (slot stays as
//!    `DefaultDriver`) still works end-to-end — `tell` reaches the user
//!    `Receive` handler because the Core trio is still composed and
//!    `DefaultDriver` contributes no events of its own.
//! 6. `combine_drivers!(A, B)` followed by a single `with_driver(combined)`
//!    drives both child drivers' `apply` for events from their respective
//!    sources — the canonical multi-driver pattern post-#57.
//! 7. Exhaustion contract: a user driver that returns `None` from `next` does
//!    NOT stop the process. `lifecycle.rs` wraps the user driver in
//!    `FusedDriver`, which absorbs `None` and stays pending, so the Core trio
//!    keeps the process alive. This preserves the pre-spec `DynDriverSet`
//!    contract where an exhausted driver was silently removed.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::combine_drivers;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{DefaultDriver, Driver, Process, ProcessContext, Receive};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Fixtures: a tick-style process whose ticks/started/stopped state is
// observable through shared atomics so the lifecycle behavior can be asserted
// without poking into the runtime.
// ---------------------------------------------------------------------------

struct TickProcess {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Process for TickProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

fn tick_props(
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) -> Props<TickProcess> {
    Props::new(move || TickProcess {
        ticks: ticks.clone(),
        started: started.clone(),
        stopped: stopped.clone(),
    })
}

fn fresh_state() -> (Arc<AtomicU32>, Arc<AtomicBool>, Arc<AtomicBool>) {
    (
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    )
}

async fn wait_for_flag(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_count(counter: &AtomicU32, expected: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (last value = {})",
            counter.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Custom drivers used across the integration tests.
//
// `supports_idle_timeout = false` for every custom driver so idle timeouts
// never interfere with assertions about *what stopped the process*. The
// I6 test depends on this: the only way the process can stop is `None`
// propagation, not the idle timer firing.
// ---------------------------------------------------------------------------

struct UnitChannelDriver {
    rx: mpsc::Receiver<()>,
}

impl Driver<TickProcess> for UnitChannelDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }
    async fn apply(
        &mut self,
        state: &mut TickProcess,
        _ctx: &mut ProcessContext<TickProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        state.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

struct IntChannelDriver {
    rx: mpsc::Receiver<i32>,
    counter: Arc<AtomicU32>,
}

impl Driver<TickProcess> for IntChannelDriver {
    type Event = i32;
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }
    async fn apply(
        &mut self,
        _state: &mut TickProcess,
        _ctx: &mut ProcessContext<TickProcess>,
        _ev: i32,
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

struct StrChannelDriver {
    rx: mpsc::Receiver<String>,
    counter: Arc<AtomicU32>,
}

impl Driver<TickProcess> for StrChannelDriver {
    type Event = String;
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }
    async fn apply(
        &mut self,
        _state: &mut TickProcess,
        _ctx: &mut ProcessContext<TickProcess>,
        _ev: String,
    ) -> Result<(), HandlerError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

// ===========================================================================
// Type-level checks. These do not execute; they exist purely to anchor the
// type signature of the new API. If any of them stops compiling, the type
// shape of Issue #57's contract has regressed and downstream callers will
// silently shift away from the spec.
// ===========================================================================

// Pin: `Props::new` returns `Props<P, DefaultDriver>` — the default type
// parameter is exactly `DefaultDriver`, not some private alias and not a
// type-erased Vec wrapper. This is the contract that lets every existing
// `Props::new(...).with_name(...)` site keep compiling without naming a
// driver type.
#[allow(dead_code)]
fn _props_new_returns_default_driver_slot() {
    let (ticks, started, stopped) = fresh_state();
    let _p: Props<TickProcess, DefaultDriver> = tick_props(ticks, started, stopped);
}

// Pin: `with_driver<D2>` is type-transitioning. The input type's `D` is
// dropped and the output type's `D` is the freshly supplied driver. If
// `with_driver` ever regresses to `fn with_driver(self, d: D) -> Self`
// (the pre-spec append-to-Vec shape), this rebinding stops compiling
// because `Props<P, DefaultDriver>` is not `Props<P, UnitChannelDriver>`.
#[allow(dead_code)]
fn _with_driver_transitions_slot_type_from_default_to_custom() {
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let props: Props<TickProcess, DefaultDriver> = tick_props(ticks, started, stopped);
    let _replaced: Props<TickProcess, UnitChannelDriver> =
        props.with_driver(UnitChannelDriver { rx });
}

// Pin: calling `with_driver` twice REPLACES the slot rather than appending.
// The final type is `Props<P, second-driver-type>`, not anything that wraps
// the first driver. This is the type-level proof of the §I6 / R3 contract
// that multi-driver use MUST go through `combine_drivers!`.
#[allow(dead_code)]
fn _with_driver_called_twice_keeps_only_the_second_drivers_type() {
    let (ticks, started, stopped) = fresh_state();
    let (_tx_first, rx_first) = mpsc::channel::<()>(4);
    let (_tx_second, rx_second) = mpsc::channel::<i32>(4);

    let after_first = tick_props(ticks, started, stopped).with_driver(UnitChannelDriver {
        rx: rx_first,
    });
    let _after_second: Props<TickProcess, IntChannelDriver> =
        after_first.with_driver(IntChannelDriver {
            rx: rx_second,
            counter: Arc::new(AtomicU32::new(0)),
        });
}

// Pin: after `with_driver`, the remaining `with_*` setters preserve `D`
// (only `with_driver` is type-transitioning). If `with_name` ever
// accidentally widens or rebinds `D`, this annotation stops compiling.
#[allow(dead_code)]
fn _with_driver_then_with_name_preserves_driver_type_parameter() {
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let _: Props<TickProcess, UnitChannelDriver> = tick_props(ticks, started, stopped)
        .with_driver(UnitChannelDriver { rx })
        .with_name(ProcessName::new("after-with-driver"));
}

// Pin: combining drivers via `combine_drivers!` and feeding the single
// composite into `with_driver` produces a `Props` whose `D` is exactly the
// composite type. This is the only API-level path the spec allows for
// "multiple drivers" — there is no `with_driver` overload that accepts a
// Vec or tuple, and no `add_driver` that appends.
#[allow(dead_code)]
fn _combine_drivers_then_with_driver_yields_combined_d_type() {
    use nitinol_runtime::process::Combine;
    let (ticks, started, stopped) = fresh_state();
    let (_int_tx, int_rx) = mpsc::channel::<i32>(4);
    let (_str_tx, str_rx) = mpsc::channel::<String>(4);

    let combined = combine_drivers!(
        IntChannelDriver {
            rx: int_rx,
            counter: Arc::new(AtomicU32::new(0)),
        },
        StrChannelDriver {
            rx: str_rx,
            counter: Arc::new(AtomicU32::new(0)),
        }
    );
    let _: Props<TickProcess, Combine<IntChannelDriver, StrChannelDriver>> =
        tick_props(ticks, started, stopped).with_driver(combined);
}

// ===========================================================================
// Behavior tests
// ===========================================================================

// ---------------------------------------------------------------------------
// A `Receive`-only process spawned with no `with_driver` call gets the Core
// trio for free and `tell` succeeds. This guards against `DefaultDriver`
// ever being implemented in a way that shadows / replaces the Core
// `MessageDriver`.
// ---------------------------------------------------------------------------

struct TellableProcess {
    received: Arc<AtomicU32>,
}

impl Process for TellableProcess {}

struct Ping;

impl Receive<Ping> for TellableProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Ping,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.received.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Given a `Props::new(...)` value with no `with_driver` call (slot stays
/// as `DefaultDriver`),
/// when the process is spawned and `tell(Ping)` is invoked,
/// then the user `Receive<Ping>` handler runs — proving the Core
/// `MessageDriver` is still composed alongside `DefaultDriver`. If
/// `DefaultDriver` had replaced the Core slot (rather than living next to
/// it), `tell` would deadlock and `wait_for_count` would time out.
#[tokio::test]
async fn default_driver_slot_keeps_core_message_driver_alive_for_tell() {
    let system = ProcessSystem::new().await;
    let received = Arc::new(AtomicU32::new(0));
    let props = Props::new({
        let received = received.clone();
        move || TellableProcess {
            received: received.clone(),
        }
    });

    let proxy = system.spawn(props).await;
    proxy
        .tell(Ping)
        .await
        .expect("tell must succeed on a Props that never called with_driver");

    wait_for_count(&received, 1, "received == 1 from DefaultDriver-only process").await;
}

// ---------------------------------------------------------------------------
// `with_driver(custom)` composes the user driver on top of the Core trio so
// custom events drive `apply` AND mailbox traffic still flows. The Core
// trio is never replaced by the user's driver.
// ---------------------------------------------------------------------------

/// Given a `Props::with_driver(UnitChannelDriver)` value,
/// when the process is spawned and three events are pushed onto the
/// driver's channel,
/// then the driver's `apply` runs three times — proving `with_driver`
/// installs the slot AND composes it with the Core `MessageDriver` (the
/// `on_start` hook ran first, demonstrating the lifecycle loop is alive).
#[tokio::test]
async fn with_driver_runs_custom_apply_for_each_delivered_event() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (tx, rx) = mpsc::channel::<()>(4);

    let _proxy = system
        .spawn(
            tick_props(Arc::clone(&ticks), Arc::clone(&started), stopped)
                .with_driver(UnitChannelDriver { rx }),
        )
        .await;
    wait_for_flag(&started, "on_start under with_driver").await;

    tx.send(()).await.expect("driver send 1");
    tx.send(()).await.expect("driver send 2");
    tx.send(()).await.expect("driver send 3");

    wait_for_count(&ticks, 3, "ticks == 3 from with_driver apply").await;
}

// ---------------------------------------------------------------------------
// The canonical multi-driver pattern: build a single composite with
// `combine_drivers!`, then hand it to a single `with_driver` call.
// ---------------------------------------------------------------------------

/// Given a composite driver built with `combine_drivers!(IntDriver, StrDriver)`
/// and installed via a single `with_driver(combined)`,
/// when events are sent on both child channels,
/// then both children's `apply` run — proving the composition is preserved
/// across the `with_driver` boundary (the `Combine` tree is not flattened,
/// degraded to a single child, or otherwise mishandled).
#[tokio::test]
async fn combine_drivers_then_with_driver_dispatches_events_to_both_children() {
    let system = ProcessSystem::new().await;
    let (_ticks, started, stopped) = fresh_state();
    let int_cnt = Arc::new(AtomicU32::new(0));
    let str_cnt = Arc::new(AtomicU32::new(0));
    let (int_tx, int_rx) = mpsc::channel::<i32>(4);
    let (str_tx, str_rx) = mpsc::channel::<String>(4);

    let combined = combine_drivers!(
        IntChannelDriver {
            rx: int_rx,
            counter: int_cnt.clone(),
        },
        StrChannelDriver {
            rx: str_rx,
            counter: str_cnt.clone(),
        }
    );

    let _proxy = system
        .spawn(
            tick_props(Arc::new(AtomicU32::new(0)), Arc::clone(&started), stopped)
                .with_driver(combined),
        )
        .await;
    wait_for_flag(&started, "on_start under combine+with_driver").await;

    int_tx.send(1).await.expect("int send 1");
    int_tx.send(2).await.expect("int send 2");
    str_tx.send("a".into()).await.expect("str send 1");

    wait_for_count(&int_cnt, 2, "int counter == 2 from combined Left side").await;
    wait_for_count(&str_cnt, 1, "str counter == 1 from combined Right side").await;
}

// ---------------------------------------------------------------------------
// Builder chain check: `with_driver` followed by `with_name` produces a
// process that is BOTH driven by the custom driver AND discoverable under
// the supplied alias. This guards against the type-transition breaking the
// downstream `with_*` setters.
// ---------------------------------------------------------------------------

/// Given a builder chain `Props::with_driver(d).with_name(n)`,
/// when the process is spawned,
/// then `lookup_by_name(n)` resolves to the same `Pid` that `spawn`
/// returned — proving the name is plumbed through the type-transitioning
/// builder without being lost.
#[tokio::test]
async fn with_driver_then_with_name_registers_alias_under_supplied_name() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (_tx, rx) = mpsc::channel::<()>(4);
    let name = ProcessName::new("issue-57-with-driver-then-name");

    let proxy = system
        .spawn(
            tick_props(ticks, Arc::clone(&started), stopped)
                .with_driver(UnitChannelDriver { rx })
                .with_name(name.clone()),
        )
        .await;
    wait_for_flag(&started, "on_start under with_driver().with_name()").await;

    let found = system.lookup_by_name(&name).await.expect(
        "with_name() must still register the alias after the type-transitioning with_driver()",
    );
    let typed = found
        .downcast::<TickProcess>()
        .expect("alias must resolve to the same TickProcess proxy");
    assert_eq!(typed.pid(), proxy.pid());
}

// ---------------------------------------------------------------------------
// Exhaustion contract: a user driver returning `None` from `next` must NOT
// stop the process. `lifecycle.rs` wraps the user driver in `FusedDriver`
// which absorbs the `None` and stays forever-pending, so the Core trio
// (MessageDriver + PipeDriver + StashDriver) keeps the process alive.
//
// This preserves the pre-#57 `DynDriverSet` contract where an exhausted
// driver was silently removed and the remaining Core trio continued operating.
//
// `with_idle_timeout(Persistent)` ensures the idle-timeout timer is disarmed
// so the only way the process can stop is an explicit Stop/Poison signal —
// not `None` propagation from `FusedDriver(ExhaustibleDriver)`.
// ---------------------------------------------------------------------------

struct ExhaustibleDriver {
    rx: mpsc::Receiver<()>,
}

impl Driver<TickProcess> for ExhaustibleDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }
    async fn apply(
        &mut self,
        _state: &mut TickProcess,
        _ctx: &mut ProcessContext<TickProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// Given a process spawned with a single `with_driver(ExhaustibleDriver)`
/// and `IdleTimeout::Persistent` (so no idle timer is armed),
/// when the driver's channel sender is dropped (causing `next()` to return
/// `None`),
/// then the process does NOT stop — `on_stop` is not called — because
/// `lifecycle.rs` wraps the user driver in `FusedDriver` which converts the
/// `None` into a forever-pending future. The process remains alive on the
/// Core trio until an explicit `Stop` signal arrives.
#[tokio::test]
async fn exhausted_with_driver_keeps_process_alive_on_core_trio() {
    let system = ProcessSystem::new().await;
    let (ticks, started, stopped) = fresh_state();
    let (tx, rx) = mpsc::channel::<()>(4);

    let proxy = system
        .spawn(
            tick_props(ticks, Arc::clone(&started), Arc::clone(&stopped))
                .with_driver(ExhaustibleDriver { rx })
                .with_idle_timeout(IdleTimeout::Persistent),
        )
        .await;
    wait_for_flag(&started, "on_start before driver exhaustion").await;

    // Exhaust the user driver.
    drop(tx);

    // Give any `None`-propagation sufficient time to terminate the process
    // if it were going to. A process that incorrectly stops due to driver
    // exhaustion would have set `stopped` well within this window.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !stopped.load(Ordering::SeqCst),
        "process must NOT stop when user driver is exhausted — Core trio must keep it alive"
    );

    // Confirm the process is still responsive to an explicit Stop signal.
    proxy
        .stop()
        .await
        .expect("stop must succeed: process must still be alive after driver exhaustion");
    wait_for_flag(&stopped, "on_stop after explicit Stop signal").await;
}
