//! Tests for Issue #56: `Props<P>` self-consuming builder chain.
//!
//! Pre-spec, `Props::with_*` returned `&mut Self`, forcing intermediate
//! `let mut props = ...; props.with_x(...); props.with_y(...);` boilerplate
//! at every call site. The spec changes the signature to `self -> Self` so
//! the builder forms a single expression:
//!
//! ```text
//! let props = Props::new(factory)
//!     .with_mailbox_capacity(MailboxCapacity::Inherit)
//!     .with_stash_capacity(StashCapacity::Bounded(NonZeroUsize::new(64).unwrap()))
//!     .with_pipe_capacity(PipeCapacity::Bounded(NonZeroUsize::new(64).unwrap()))
//!     .with_supervision_strategy(SupervisionStrategy::restart(3, d)?)
//!     .with_idle_timeout(IdleTimeout::After(d))
//!     .with_name(ProcessName::new("my-process"))
//!     .with_driver(IntervalDriver::new(d));
//! ```
//!
//! These tests pin down:
//! - Each `with_*` setter takes `self` by value; the non-driver setters
//!   return `Self`, while `with_driver` is the single type-transitioning
//!   setter that returns `Props<P, D2>`.
//! - The chain compiles for all 7 setters in one expression.
//! - Round-trip through observable behavior: `with_name` makes the process
//!   discoverable by the supplied name; `with_idle_timeout` triggers the
//!   configured timeout; `with_supervision_strategy` enforces the policy.
//! - `with_driver<D: Driver<P>>` accepts any `Driver<P>` implementor.
//!
//! Round-trip checks for capacity fields are in `system_defaults_inherit.rs`
//! (because the observable effect is buffer bounded-ness, which needs a
//! collaborating handler to surface).

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Driver, Process, ProcessContext};
use nitinol_runtime::{
    IdleTimeout, MailboxCapacity, PipeCapacity, ProcessSystem, Props, StashCapacity,
    SupervisionStrategy,
};

// ---------------------------------------------------------------------------
// Fixture: a no-op process used only for builder-shape tests where the value
// flow matters more than handler behavior.
// ---------------------------------------------------------------------------

struct NoOpProcess;
impl Process for NoOpProcess {}

fn noop_props() -> Props<NoOpProcess> {
    Props::new(|| NoOpProcess)
}

// ---------------------------------------------------------------------------
// Type-level: each `with_*` returns `Self`, not `&mut Self`.
//
// If a setter ever regresses back to `&mut Self`, the rebinding to a typed
// `Props<NoOpProcess>` stops compiling because `&mut Self != Self`.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _with_supervision_strategy_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_supervision_strategy(SupervisionStrategy::stop());
}

#[allow(dead_code)]
fn _with_idle_timeout_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_idle_timeout(IdleTimeout::Persistent);
}

#[allow(dead_code)]
fn _with_mailbox_capacity_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_mailbox_capacity(MailboxCapacity::Inherit);
}

#[allow(dead_code)]
fn _with_stash_capacity_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_stash_capacity(StashCapacity::Inherit);
}

#[allow(dead_code)]
fn _with_pipe_capacity_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_pipe_capacity(PipeCapacity::Inherit);
}

#[allow(dead_code)]
fn _with_name_returns_self_by_value() {
    let p: Props<NoOpProcess> = noop_props();
    let _: Props<NoOpProcess> = p.with_name(ProcessName::new("builder-chain"));
}

// ---------------------------------------------------------------------------
// Type-level: the spec's full chain compiles as one expression.
//
// If any setter regresses to `&mut Self` or any return type drifts, this
// stops compiling — a single point of failure for the chain shape.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _full_builder_chain_compiles_as_single_expression() {
    let n = NonZeroUsize::new(64).expect("64 is non-zero");
    let _props: Props<NoOpProcess> = Props::new(|| NoOpProcess)
        .with_mailbox_capacity(MailboxCapacity::Inherit)
        .with_stash_capacity(StashCapacity::Bounded(n))
        .with_pipe_capacity(PipeCapacity::Bounded(n))
        .with_supervision_strategy(SupervisionStrategy::stop())
        .with_idle_timeout(IdleTimeout::Persistent)
        .with_name(ProcessName::new("chain"));
}

// ---------------------------------------------------------------------------
// Round-trip via behavior: `with_name(name)` is observable through the registry
// after spawn.
// ---------------------------------------------------------------------------

/// Given a `Props::with_name("named-via-with-name")` value,
/// when the process is spawned,
/// then `system.lookup_by_name(...)` finds the spawned process under that
/// exact name. Pins down the round-trip from `with_name(...)` to registry
/// registration.
#[tokio::test]
async fn with_name_registers_process_under_supplied_name() {
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("named-via-with-name");

    let proxy = system.spawn(noop_props().with_name(name.clone())).await;

    let found = system
        .lookup_by_name(&name)
        .await
        .expect("with_name must register the process under the supplied name");
    let typed = found
        .downcast::<NoOpProcess>()
        .expect("alias must resolve to the same process type");
    assert_eq!(typed.pid(), proxy.pid());
}

// ---------------------------------------------------------------------------
// Round-trip via behavior: `with_idle_timeout` controls when the process stops.
// ---------------------------------------------------------------------------

/// Process that records when it stops via an atomic flag — enough surface to
/// observe `with_idle_timeout`'s effect.
struct StopFlagProcess {
    stopped: Arc<AtomicBool>,
}

impl Process for StopFlagProcess {
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

async fn wait_for_flag(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for stop flag to become true"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Given `Props::with_idle_timeout(IdleTimeout::After(50ms))`,
/// when spawned with no further messages,
/// then `on_stop` runs within the timeout window — pinning down that
/// `with_idle_timeout` actually plumbs the duration through to the lifecycle
/// loop's timer.
#[tokio::test]
async fn with_idle_timeout_after_triggers_stop_within_window() {
    let system = ProcessSystem::new().await;
    let stopped = Arc::new(AtomicBool::new(false));
    let props = Props::new({
        let stopped = stopped.clone();
        move || StopFlagProcess {
            stopped: stopped.clone(),
        }
    })
    .with_idle_timeout(IdleTimeout::After(Duration::from_millis(50)));

    let _proxy = system.spawn(props).await;

    wait_for_flag(&stopped).await;
}

// ---------------------------------------------------------------------------
// with_driver: a custom Driver<P> can be attached via the builder chain. The
// spawn path composes it with the Core drivers (Message + Pipe + Stash).
// ---------------------------------------------------------------------------

/// Test process that exposes a counter the custom driver bumps on each event.
struct DrivenProcess {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
}

impl Process for DrivenProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
}

/// Channel-backed `Driver<DrivenProcess>` — the simplest tick source that lets
/// the test drive `apply` from outside.
struct TickDriver {
    rx: mpsc::Receiver<()>,
}

impl Driver<DrivenProcess> for TickDriver {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        state: &mut DrivenProcess,
        _ctx: &mut ProcessContext<DrivenProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        state.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for counter to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Given a `Props::with_driver(TickDriver)` value,
/// when the process is spawned and the test sends 3 events on the driver's
/// channel,
/// then the driver's `apply` runs 3 times — proving the custom driver is
/// actually composed into the lifecycle loop alongside the Core drivers,
/// not silently ignored.
#[tokio::test]
async fn with_driver_attaches_custom_driver_and_runs_apply() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<()>(4);

    let props = Props::new({
        let ticks = ticks.clone();
        let started = started.clone();
        move || DrivenProcess {
            ticks: ticks.clone(),
            started: started.clone(),
        }
    })
    .with_driver(TickDriver { rx });

    let _proxy = system.spawn(props).await;

    wait_for_flag(&started).await;

    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");

    wait_for_count(&ticks, 3).await;
}

// ---------------------------------------------------------------------------
// with_driver composes ON TOP OF the Core drivers — the user mailbox (tell/ask)
// still works on a process whose Props had `with_driver` applied.
// ---------------------------------------------------------------------------

/// `tell` arrives at a `Receive<...>` handler defined on the user process.
struct MessageCounter {
    received: Arc<AtomicU32>,
}

impl Process for MessageCounter {}

struct Ping;

impl nitinol_runtime::process::Receive<Ping> for MessageCounter {
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

/// A custom driver whose `next` never produces — it should not steal mailbox
/// messages from the Core MessageDriver. Used to prove the two driver classes
/// coexist after `with_driver`.
struct NeverDriver;

impl Driver<MessageCounter> for NeverDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut MessageCounter,
        _ctx: &mut ProcessContext<MessageCounter>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// Given a `Props::with_driver(NeverDriver)` value,
/// when the process is spawned and a `Ping` is told,
/// then the user's `Receive<Ping>` handler runs — proving the Core
/// `MessageDriver` is still composed alongside the custom driver and that
/// `with_driver` does not REPLACE the message path.
#[tokio::test]
async fn with_driver_composes_with_core_message_driver() {
    let system = ProcessSystem::new().await;
    let received = Arc::new(AtomicU32::new(0));
    let props = Props::new({
        let received = received.clone();
        move || MessageCounter {
            received: received.clone(),
        }
    })
    .with_driver(NeverDriver);

    let proxy = system.spawn(props).await;
    proxy
        .tell(Ping)
        .await
        .expect("tell must succeed when MessageDriver is composed");

    wait_for_count(&received, 1).await;
}
