//! Tests for `ProcessContext<P>::pipe_to_self` (Issue #53).
//!
//! These tests pin down the externally observable contract of the new API:
//! - A future's `Ok` output is delivered as a typed message via the mapper.
//! - A future panic is converted into `Err(PipePanic)` and handed to the mapper.
//! - `PipePanic` formats `&'static str` / `String` panic payloads.
//! - Multiple in-flight futures are all delivered; completion order is decoupled
//!   from registration order.
//! - The current handler returns before the piped follow-up message is processed
//!   (continuation, not re-entrancy).
//! - Calling `pipe_to_self` without a composed `PipeDriver` panics
//!   immediately (programmer error, not a silent drop).
//! - The `Driver::pipe_handle` default returns `None`.
//!
//! `ProcessSystem::spawn` auto-composes `PipeDriver` (per the plan), so the
//! happy-path tests use `spawn`. The misuse-path test uses `spawn_with_driver`
//! with a driver that does NOT compose `PipeDriver`.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{
    Driver, PipeDriver, Process, ProcessContext, Receive,
};
use nitinol_runtime::{ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Fixture: a process that collects piped results and lets the test inspect them.
// ---------------------------------------------------------------------------

/// What the process saw from the mapper closure on each piped result.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observed {
    /// Future completed successfully with `i32`.
    Value(i32),
    /// Future panicked; payload formatted as `format!("{}", panic)`.
    Panic(String),
    /// Future panicked; payload type-id stored as a stable proof that
    /// `PipePanic::payload()` is reachable from user code.
    PanicViaPayload(String),
}

#[derive(Default)]
struct PipeProcess {
    observed: Arc<Mutex<Vec<Observed>>>,
    /// Counter incremented synchronously inside `Receive<StartPipe>` BEFORE
    /// returning, used to assert continuation semantics.
    start_pipe_returns: Arc<AtomicU32>,
}

impl Process for PipeProcess {}

/// Trigger a pipe that resolves to `value` (no panic).
struct StartPipe {
    value: i32,
}

/// Trigger a pipe whose future panics with a `&'static str` payload.
struct StartPipePanic {
    msg: &'static str,
}

/// Trigger a pipe whose future panics with a `String` payload, and whose
/// mapper extracts via `PipePanic::payload()` instead of Display.
struct StartPipePanicViaPayload {
    msg: String,
}

/// Trigger two pipes back-to-back with mixed delays. The first one sleeps
/// for `slow_ms`; the second resolves immediately. If the FuturesUnordered
/// in `PipeDriver` polls fairly, the immediate result must surface first.
struct StartTwoPipes {
    slow_ms: u64,
    slow_value: i32,
    fast_value: i32,
}

/// Message produced by the mapper. Distinct type ensures Receive routing.
struct Delivered(Observed);

/// Query: snapshot the observed log.
struct Snapshot;

/// Query: count of times the StartPipe handler returned (continuation proof).
struct StartPipeReturnCount;

impl Receive<StartPipe> for PipeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: StartPipe,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        ctx.pipe_to_self(std::future::ready(msg.value), |result| {
            // Per spec, the mapper sees Result<F::Output, PipePanic>.
            match result {
                Ok(v) => Delivered(Observed::Value(v)),
                Err(p) => Delivered(Observed::Panic(format!("{p}"))),
            }
        });
        self.start_pipe_returns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Receive<StartPipePanic> for PipeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: StartPipePanic,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let payload = msg.msg;
        // Async block that panics during poll. PipeDriver catches the unwind.
        let fut = async move {
            panic!("{}", payload);
        };
        ctx.pipe_to_self(fut, move |result| match result {
            Ok(()) => Delivered(Observed::Value(-1)),
            Err(p) => Delivered(Observed::Panic(format!("{p}"))),
        });
        Ok(())
    }
}

impl Receive<StartPipePanicViaPayload> for PipeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: StartPipePanicViaPayload,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let payload: String = msg.msg;
        let fut = async move {
            // String payload (not &'static str): exercises both branches.
            std::panic::panic_any(payload);
        };
        ctx.pipe_to_self(fut, move |result| match result {
            Ok(()) => Delivered(Observed::Value(-1)),
            Err(p) => {
                // Pull the payload out directly. Per the plan, `PipePanic`
                // exposes a `payload()` accessor returning a trait object
                // reference that can be downcast to the concrete panic type.
                let any_ref: &(dyn std::any::Any + Send) = p.payload();
                let raw = if let Some(s) = any_ref.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = any_ref.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else {
                    "<non-string panic payload>".to_string()
                };
                Delivered(Observed::PanicViaPayload(raw))
            }
        });
        Ok(())
    }
}

impl Receive<StartTwoPipes> for PipeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: StartTwoPipes,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // First: slow future.
        let slow_ms = msg.slow_ms;
        let slow_value = msg.slow_value;
        ctx.pipe_to_self(
            async move {
                tokio::time::sleep(Duration::from_millis(slow_ms)).await;
                slow_value
            },
            |result| Delivered(Observed::Value(result.unwrap())),
        );

        // Second: immediate future. The lifecycle loop must be able to
        // make progress on this one even while the slow one is in-flight.
        let fast_value = msg.fast_value;
        ctx.pipe_to_self(std::future::ready(fast_value), |result| {
            Delivered(Observed::Value(result.unwrap()))
        });

        Ok(())
    }
}

impl Receive<Delivered> for PipeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: Delivered,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.observed.lock().await.push(msg.0);
        Ok(())
    }
}

impl Receive<Snapshot> for PipeProcess {
    type Response = Vec<Observed>;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: Snapshot,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<Vec<Observed>, std::convert::Infallible> {
        Ok(self.observed.lock().await.clone())
    }
}

impl Receive<StartPipeReturnCount> for PipeProcess {
    type Response = u32;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: StartPipeReturnCount,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.start_pipe_returns.load(Ordering::SeqCst))
    }
}

fn make_props(
    observed: Arc<Mutex<Vec<Observed>>>,
    start_pipe_returns: Arc<AtomicU32>,
) -> Props<PipeProcess> {
    Props::new(move || PipeProcess {
        observed: observed.clone(),
        start_pipe_returns: start_pipe_returns.clone(),
    })
}

/// Poll the actor's observed log until it contains at least `expected_len`
/// entries, with a 5-second timeout. Returns the snapshot.
async fn wait_for_at_least(
    proxy: &nitinol_runtime::process::ProcessProxy<PipeProcess>,
    expected_len: usize,
) -> Vec<Observed> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = proxy.ask(Snapshot).await.expect("ask Snapshot");
        if snap.len() >= expected_len {
            return snap;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for at least {expected_len} piped messages; got {snap:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Happy path: success result is delivered as a typed message.
// ---------------------------------------------------------------------------

/// Given a process spawned via `system.spawn` (PipeDriver auto-composed),
/// when its handler calls `ctx.pipe_to_self(ready(42), |Ok(v)| Delivered(v))`,
/// then the same actor receives `Delivered(42)` as a follow-up message.
#[tokio::test]
async fn pipe_to_self_delivers_future_value_as_mapped_message() {
    // Given
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    // When
    proxy
        .tell(StartPipe { value: 42 })
        .await
        .expect("tell StartPipe must succeed");

    // Then: the mapped message arrives, exactly once, with the value.
    let snap = wait_for_at_least(&proxy, 1).await;
    assert_eq!(
        snap,
        vec![Observed::Value(42)],
        "exactly one Delivered(Value(42)) must arrive via pipe_to_self"
    );
}

// ---------------------------------------------------------------------------
// Continuation semantics: the StartPipe handler returns BEFORE the piped
// follow-up message is processed.
// ---------------------------------------------------------------------------

/// Given a handler that calls `pipe_to_self` and then increments a counter
/// just before returning, when StartPipe is delivered, then the counter
/// reaches 1 BEFORE the follow-up message lands. This proves the piped
/// future runs as a continuation (after the current handler returns), not
/// reentrantly inside it.
#[tokio::test]
async fn pipe_to_self_runs_as_continuation_not_reentrantly() {
    // Given
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    // When
    proxy
        .tell(StartPipe { value: 7 })
        .await
        .expect("tell StartPipe must succeed");
    // Drain via ask barrier: this ask only completes after the StartPipe
    // handler has returned (the mailbox is single-threaded).
    let returned = proxy
        .ask(StartPipeReturnCount)
        .await
        .expect("ask must succeed");

    // Then: the handler returned at least once before the barrier ask.
    assert!(
        returned >= 1,
        "StartPipe handler must return before subsequent messages are processed (got {returned})"
    );

    // And: the piped follow-up eventually lands.
    let snap = wait_for_at_least(&proxy, 1).await;
    assert_eq!(snap, vec![Observed::Value(7)]);
}

// ---------------------------------------------------------------------------
// Multiple in-flight futures: completion order is decoupled from registration.
// ---------------------------------------------------------------------------

/// Given two piped futures — one slow (sleeps), one immediate —,
/// when both are registered in that order via a single handler,
/// then BOTH mapped messages arrive (no message lost), and the immediate
/// future's result is observed first.
#[tokio::test]
async fn pipe_to_self_fast_future_completes_before_slow() {
    // Given
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    // When
    proxy
        .tell(StartTwoPipes {
            slow_ms: 80,
            slow_value: 1,
            fast_value: 2,
        })
        .await
        .expect("tell StartTwoPipes must succeed");

    // Then: both Delivered messages eventually arrive.
    let snap = wait_for_at_least(&proxy, 2).await;
    assert_eq!(
        snap.len(),
        2,
        "exactly 2 piped results must be observed (no duplicates, no losses); got {snap:?}"
    );
    assert!(
        snap.contains(&Observed::Value(1)),
        "slow future's value=1 must arrive"
    );
    assert!(
        snap.contains(&Observed::Value(2)),
        "fast future's value=2 must arrive"
    );

    // And: the fast value lands first because FuturesUnordered yields it
    // as soon as it is Ready, while the slow one is still sleeping.
    assert_eq!(
        snap[0],
        Observed::Value(2),
        "immediate future must surface before the slow one; got order {snap:?}"
    );
    assert_eq!(
        snap[1],
        Observed::Value(1),
        "slow future surfaces after the immediate one; got order {snap:?}"
    );
}

// ---------------------------------------------------------------------------
// Panic capture: a future that panics during poll is delivered as
// `Err(PipePanic)` to the mapper, not propagated out of the lifecycle loop.
// ---------------------------------------------------------------------------

/// Given a piped future whose body panics with a `&'static str`,
/// when it is polled by the PipeDriver,
/// then the mapper receives `Err(PipePanic)` and `format!("{panic}")` includes
/// the original message — proving the panic was caught and surfaced, not
/// crashing the actor.
#[tokio::test]
async fn pipe_to_self_passes_panic_to_mapper_as_pipe_panic() {
    // Given
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    // When
    proxy
        .tell(StartPipePanic { msg: "boom-static" })
        .await
        .expect("tell StartPipePanic must succeed");

    // Then: a single Panic observation arrives whose Display contains the msg.
    let snap = wait_for_at_least(&proxy, 1).await;
    assert_eq!(
        snap.len(),
        1,
        "exactly one Delivered must arrive (panic must not duplicate or drop)"
    );
    match &snap[0] {
        Observed::Panic(s) => assert!(
            s.contains("boom-static"),
            "PipePanic Display must include the &'static str panic payload \
             so users can log it; got {s:?}"
        ),
        other => panic!("expected Observed::Panic, got {other:?}"),
    }

    // And: the actor is still alive after the captured panic — the handler
    // for a normal StartPipe must still respond.
    proxy
        .tell(StartPipe { value: 99 })
        .await
        .expect("the actor must remain alive after a piped future panic");
    let snap2 = wait_for_at_least(&proxy, 2).await;
    assert!(
        snap2.iter().any(|o| matches!(o, Observed::Value(99))),
        "post-panic tell must still be processed; snapshot = {snap2:?}"
    );
}

/// Given a piped future that panics with a `String` payload,
/// when the mapper accesses `PipePanic::payload()` directly,
/// then it can downcast the boxed Any to `String` — proving the raw payload
/// is reachable from user code (not only via Display).
#[tokio::test]
async fn pipe_panic_payload_is_accessible_via_payload_accessor() {
    // Given
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    // When
    proxy
        .tell(StartPipePanicViaPayload {
            msg: "boom-owned".to_string(),
        })
        .await
        .expect("tell StartPipePanicViaPayload must succeed");

    // Then
    let snap = wait_for_at_least(&proxy, 1).await;
    assert_eq!(snap.len(), 1);
    assert_eq!(
        snap[0],
        Observed::PanicViaPayload("boom-owned".to_string()),
        "PipePanic::payload() must expose the raw panic payload for typed downcast"
    );
}

// ---------------------------------------------------------------------------
// Issue #56 inversion: under the unified spawn entry, the Core `PipeDriver`
// is ALWAYS composed, so `ctx.pipe_to_self` never panics on a "missing
// driver" basis. The probe here is now a positive check that the always-
// composed driver actually delivers the piped follow-up.
// ---------------------------------------------------------------------------

/// Process used only to host the "no PipeDriver" probe driver. Its handlers
/// are never called — the driver itself drives behavior via `apply`.
struct ProbeProcess;

impl Process for ProbeProcess {}

/// A "tick" message — defined only so `ctx.pipe_to_self`'s mapper has a valid
/// `M: Receive<M>` to map to. The handler is never invoked in the misuse test
/// because the call site panics before the future is enqueued.
struct Tick;

impl Receive<Tick> for ProbeProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: Tick,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

/// Driver that fires exactly one event, calls `ctx.pipe_to_self` inside
/// `apply`, catches the synchronous panic via `catch_unwind`, and records
/// the panic message (or "ok-no-panic" if it didn't panic).
struct ProbeDriver {
    rx: mpsc::Receiver<()>,
    captured: Arc<Mutex<Option<String>>>,
    ok_path_taken: Arc<Mutex<bool>>,
}

impl Driver<ProbeProcess> for ProbeDriver {
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }

    async fn apply(
        &mut self,
        _state: &mut ProbeProcess,
        ctx: &mut ProcessContext<ProbeProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        // `pipe_to_self` panics synchronously before any future is enqueued,
        // so wrapping the sync call site in `catch_unwind` captures it.
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            ctx.pipe_to_self(std::future::ready(0_u32), |_| Tick);
        }));

        match result {
            Ok(()) => {
                *self.ok_path_taken.lock().await = true;
            }
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic payload>".to_string()
                };
                *self.captured.lock().await = Some(msg);
            }
        }
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

/// Given a process spawned with `Props::with_driver(ProbeDriver)`,
/// when the driver's `apply` calls `ctx.pipe_to_self(...)`,
/// then the always-composed Core `PipeDriver` accepts the future and the
/// call returns normally — no panic, no silent drop on the happy path.
#[tokio::test]
async fn pipe_to_self_does_not_panic_under_unified_spawn_entry() {
    // Given
    let system = ProcessSystem::new().await;
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let ok_taken: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let (tx, rx) = mpsc::channel::<()>(4);

    let props = Props::new(|| ProbeProcess).with_driver(ProbeDriver {
        rx,
        captured: captured.clone(),
        ok_path_taken: ok_taken.clone(),
    });

    let _proxy = system.spawn(props).await;

    // When: drive the apply path once.
    tx.send(()).await.expect("driver source send");

    // Then: the ok-path flag flips and no panic message is recorded.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if *ok_taken.lock().await {
            break;
        }
        if let Some(msg) = captured.lock().await.clone() {
            panic!(
                "pipe_to_self panicked under the unified spawn entry, \
                 but the Core PipeDriver is supposed to be always composed: \
                 captured panic message = {msg:?}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pipe_to_self ok-path flag to flip"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// `Driver::pipe_handle` default implementation returns `None`. Existing
// driver implementations (MessageDriver-pub(crate), the external
// IntervalDriver, custom test drivers) must not need to implement it.
// ---------------------------------------------------------------------------

struct NoPipeDriver;

impl Driver<ProbeProcess> for NoPipeDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut ProbeProcess,
        _ctx: &mut ProcessContext<ProbeProcess>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
}

/// Given a Driver implementation that does NOT override `pipe_handle`,
/// when `pipe_handle()` is called,
/// then it returns `None` — proving the default impl preserves backwards
/// compatibility for existing Drivers (MessageDriver, IntervalDriver, ...).
#[tokio::test]
async fn driver_default_pipe_handle_is_none() {
    let driver = NoPipeDriver;
    assert!(
        driver.pipe_handle().is_none(),
        "Driver::pipe_handle default impl must return None so existing \
         driver impls remain unchanged after Combine/Pipe refactor"
    );
}

/// Given a freshly constructed `PipeDriver<P>`,
/// when `pipe_handle()` is called,
/// then it returns `Some(...)` — proving PipeDriver is the source of the
/// handle that `Combine` will surface to the lifecycle loop.
#[tokio::test]
async fn pipe_driver_pipe_handle_returns_some() {
    let driver: PipeDriver<ProbeProcess> = PipeDriver::new();
    assert!(
        driver.pipe_handle().is_some(),
        "PipeDriver must override pipe_handle() to return Some(handle); \
         otherwise ctx.pipe_to_self can never be wired"
    );
}

// ---------------------------------------------------------------------------
// `PipePanic` formatting: Display must succeed for both `&'static str` and
// `String` payloads, since both are common panic payload shapes.
// ---------------------------------------------------------------------------

/// Given a piped future that panics with `&'static str`,
/// then `format!("{p}")` on the resulting `PipePanic` produces a non-empty
/// string that includes the panic message.
///
/// (Covered transitively by
/// `pipe_to_self_passes_panic_to_mapper_as_pipe_panic`, kept as a focused
/// check so a Display regression points directly at the formatter.)
#[tokio::test]
async fn pipe_panic_display_renders_static_str_payload() {
    let system = ProcessSystem::new().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let returns = Arc::new(AtomicU32::new(0));
    let proxy = system
        .spawn(make_props(observed.clone(), returns.clone()))
        .await;

    proxy
        .tell(StartPipePanic { msg: "display-check" })
        .await
        .expect("tell must succeed");

    let snap = wait_for_at_least(&proxy, 1).await;
    let formatted = match &snap[0] {
        Observed::Panic(s) => s.clone(),
        other => panic!("expected Observed::Panic, got {other:?}"),
    };
    assert!(
        !formatted.is_empty(),
        "PipePanic Display output must not be empty"
    );
    assert!(
        formatted.contains("display-check"),
        "PipePanic Display must surface the original &'static str payload; \
         got {formatted:?}"
    );
}

// The behavior tests above already pin down the mapper's argument shape
// (`Result<F::Output, PipePanic>`) at compile time, because each `Receive` impl
// matches on `Ok(_)` / `Err(_)`. A focused static check is therefore
// unnecessary here — adding one would only restate what the behavior tests
// already enforce.
