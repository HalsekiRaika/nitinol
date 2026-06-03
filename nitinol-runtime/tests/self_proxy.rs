//! Tests for the new `ProcessContext<P>` generic and `ctx.self_proxy()` API
//! introduced in Issue #52.
//!
//! Specifically, these tests pin down:
//! - `ProcessContext<P>` is generic over the hosting process type `P`
//! - `ctx.self_proxy()` returns an immutable `&ProcessProxy<P>`
//! - `self_proxy().pid()` matches `ctx.pid()`
//! - The self proxy is `Clone` and remains usable after the handler returns
//!   (foundation for Pipe-to-Self)
//! - From inside a handler, `self.self_proxy().tell(...)` queues the message
//!   on the same actor's mailbox

use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};
use nitinol_runtime::{ProcessSystem, Props};

// ---------------------------------------------------------------------------
// Test fixture: a process that captures its own `pid()` and `self_proxy().pid()`
// from inside `on_start`, and also keeps a clone of its self proxy.
// ---------------------------------------------------------------------------

struct SelfAwareProcess {
    /// `ctx.pid()` observed during `on_start`.
    observed_ctx_pid: Arc<Mutex<Option<Pid>>>,
    /// `ctx.self_proxy().pid()` observed during `on_start`.
    observed_proxy_pid: Arc<Mutex<Option<Pid>>>,
    /// A clone of the self proxy captured during `on_start`.
    captured_self_proxy: Arc<Mutex<Option<ProcessProxy<SelfAwareProcess>>>>,
    /// Count of `Increment` messages received.
    increment_count: Arc<AtomicU32>,
    /// Count of `Echo` messages received via the pipe-to-self path.
    echo_count: Arc<AtomicU32>,
}

impl Process for SelfAwareProcess {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        // `ctx.self_proxy()` must return an immutable reference. The bound
        // below would fail to compile if `self_proxy` returned `&mut` or an
        // owned value.
        let _: &ProcessProxy<Self> = ctx.self_proxy();

        let ctx_pid = ctx.pid();
        let proxy_pid = ctx.self_proxy().pid();
        let proxy_clone: ProcessProxy<Self> = ctx.self_proxy().clone();

        let observed_ctx_pid = self.observed_ctx_pid.clone();
        let observed_proxy_pid = self.observed_proxy_pid.clone();
        let captured = self.captured_self_proxy.clone();
        async move {
            *observed_ctx_pid.lock().await = Some(ctx_pid);
            *observed_proxy_pid.lock().await = Some(proxy_pid);
            *captured.lock().await = Some(proxy_clone);
        }
    }
}

struct Increment;

impl Receive<Increment> for SelfAwareProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: Increment,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.increment_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Echo;

impl Receive<Echo> for SelfAwareProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: Echo,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.echo_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// `PipeToSelf` triggers a handler that enqueues `Echo` back on the same actor
/// using `ctx.self_proxy().tell(...)`. This pins down the Pipe-to-Self pattern
/// the new API is intended to support.
struct PipeToSelf;

impl Receive<PipeToSelf> for SelfAwareProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: PipeToSelf,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        // The handler is allowed to clone the self proxy and use it to send
        // a message — the message lands on the same actor's mailbox.
        let me = ctx.self_proxy().clone();
        // `tell` only sends the message onto the mailbox; it does not block
        // on the receiver's recv. Therefore this call must not deadlock even
        // though the actor is currently inside its own handler.
        me.tell(Echo).await.expect("self tell must succeed");
        Ok(())
    }
}

/// A query that returns the current `Echo` count. Used as a barrier so the
/// outer test waits until the queued `Echo` has been processed before reading.
struct GetEchoCount;

impl Receive<GetEchoCount> for SelfAwareProcess {
    type Response = u32;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: GetEchoCount,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.echo_count.load(Ordering::SeqCst))
    }
}

/// A query that returns the current `Increment` count.
struct GetIncrementCount;

impl Receive<GetIncrementCount> for SelfAwareProcess {
    type Response = u32;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: GetIncrementCount,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        Ok(self.increment_count.load(Ordering::SeqCst))
    }
}

fn make_props(
    observed_ctx_pid: Arc<Mutex<Option<Pid>>>,
    observed_proxy_pid: Arc<Mutex<Option<Pid>>>,
    captured_self_proxy: Arc<Mutex<Option<ProcessProxy<SelfAwareProcess>>>>,
    increment_count: Arc<AtomicU32>,
    echo_count: Arc<AtomicU32>,
) -> Props<SelfAwareProcess> {
    Props::new(move || SelfAwareProcess {
        observed_ctx_pid: observed_ctx_pid.clone(),
        observed_proxy_pid: observed_proxy_pid.clone(),
        captured_self_proxy: captured_self_proxy.clone(),
        increment_count: increment_count.clone(),
        echo_count: echo_count.clone(),
    })
}

/// Poll until `Some` is observed in the shared slot, with a 5-second timeout.
async fn wait_for_some<T: Clone + Send>(slot: &Arc<Mutex<Option<T>>>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = slot.lock().await;
            if let Some(ref v) = *guard {
                return v.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Mutex<Option<T>> to become Some"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// `ctx.self_proxy()` identity properties.
// ---------------------------------------------------------------------------

/// Given a spawned process,
/// when `on_start` observes both `ctx.pid()` and `ctx.self_proxy().pid()`,
/// then both return the same PID — proving the embedded proxy is wired to
/// this exact process and not to some other.
#[tokio::test]
async fn self_proxy_pid_matches_ctx_pid_in_on_start() {
    // Given
    let system = ProcessSystem::new().await;
    let observed_ctx_pid = Arc::new(Mutex::new(None));
    let observed_proxy_pid = Arc::new(Mutex::new(None));
    let captured = Arc::new(Mutex::new(None));
    let inc = Arc::new(AtomicU32::new(0));
    let echo = Arc::new(AtomicU32::new(0));

    // When
    let proxy = system
        .spawn(make_props(
            observed_ctx_pid.clone(),
            observed_proxy_pid.clone(),
            captured.clone(),
            inc,
            echo,
        ))
        .await;
    let spawned_pid = proxy.pid();

    // Then: both PIDs observed inside on_start match the spawned process's pid.
    let ctx_pid = wait_for_some(&observed_ctx_pid).await;
    let proxy_pid = wait_for_some(&observed_proxy_pid).await;
    assert_eq!(
        ctx_pid, spawned_pid,
        "ctx.pid() inside on_start must equal the spawn-time pid"
    );
    assert_eq!(
        proxy_pid, spawned_pid,
        "ctx.self_proxy().pid() must equal the spawn-time pid"
    );
    assert_eq!(
        ctx_pid, proxy_pid,
        "ctx.self_proxy() must refer to the same process as ctx itself"
    );
}

/// Given the captured self proxy from `on_start`,
/// when the external test sends `Increment` through it,
/// then the same process receives and counts the message — proving the proxy
/// outlives the on_start scope and is independent of the spawn-time proxy.
#[tokio::test]
async fn cloned_self_proxy_routes_to_same_process() {
    // Given
    let system = ProcessSystem::new().await;
    let observed_ctx_pid = Arc::new(Mutex::new(None));
    let observed_proxy_pid = Arc::new(Mutex::new(None));
    let captured = Arc::new(Mutex::new(None));
    let inc = Arc::new(AtomicU32::new(0));
    let echo = Arc::new(AtomicU32::new(0));

    let spawn_proxy = system
        .spawn(make_props(
            observed_ctx_pid.clone(),
            observed_proxy_pid,
            captured.clone(),
            inc.clone(),
            echo,
        ))
        .await;
    let _ = wait_for_some(&observed_ctx_pid).await;
    let self_proxy = wait_for_some(&captured).await;

    // When: a tell is delivered via the proxy that was captured inside on_start
    self_proxy
        .tell(Increment)
        .await
        .expect("tell through captured self proxy must succeed");

    // Then: the spawn-time proxy can read the same actor's state via ask,
    // confirming both proxies route to one and the same process.
    let count = spawn_proxy
        .ask(GetIncrementCount)
        .await
        .expect("ask must succeed");
    assert_eq!(
        count, 1,
        "the cloned self proxy and the spawn-time proxy must address the same actor"
    );
}

// ---------------------------------------------------------------------------
// Pipe-to-Self via `ctx.self_proxy().tell(...)` inside a handler.
// ---------------------------------------------------------------------------

/// Given a process whose `Receive<PipeToSelf>` clones the self proxy and
/// uses it to send `Echo` back on the same mailbox,
/// when `PipeToSelf` is delivered,
/// then `Echo` is eventually processed by the same actor.
#[tokio::test]
async fn self_proxy_pipe_to_self_delivers_followup_message() {
    // Given
    let system = ProcessSystem::new().await;
    let observed_ctx_pid = Arc::new(Mutex::new(None));
    let captured = Arc::new(Mutex::new(None));
    let inc = Arc::new(AtomicU32::new(0));
    let echo = Arc::new(AtomicU32::new(0));

    let proxy = system
        .spawn(make_props(
            observed_ctx_pid.clone(),
            Arc::new(Mutex::new(None)),
            captured,
            inc,
            echo,
        ))
        .await;
    let _ = wait_for_some(&observed_ctx_pid).await;

    // When
    proxy
        .tell(PipeToSelf)
        .await
        .expect("tell(PipeToSelf) must succeed");

    // Then: poll via `ask(GetEchoCount)` until the follow-up Echo is observed.
    //
    // `ask` is NOT a strict barrier here: a `GetEchoCount` sent immediately
    // after `tell(PipeToSelf)` can be enqueued before the PipeToSelf handler
    // has a chance to call `self_proxy.tell(Echo)`, which means the very
    // first `ask` can race the self-tell. Polling absorbs that race while
    // still failing fast if the self-tell never lands.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let echo_count = proxy.ask(GetEchoCount).await.expect("ask must succeed");
        if echo_count == 1 {
            break;
        }
        assert_eq!(
            echo_count, 0,
            "Echo must arrive exactly once; observed {echo_count} suggests duplicate delivery"
        );
        assert!(
            Instant::now() < deadline,
            "Echo enqueued via ctx.self_proxy().tell(Echo) must be received by the same actor"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Static type-level assertions about the `self_proxy()` return signature.
// These compile-only checks force the new API surface to remain stable:
// changing the signature back to `&mut` or an owned value would fail to build.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn assert_self_proxy_returns_shared_reference<P: Process>(
    ctx: &ProcessContext<P>,
) -> &ProcessProxy<P> {
    // `ctx` is an immutable reference, yet `self_proxy()` is still callable.
    // This rules out `fn self_proxy(&mut self) -> ...` and `fn self_proxy(self)
    // -> ...`, both of which would force the function to take `&mut ctx` /
    // owned ctx and fail to compile here.
    ctx.self_proxy()
}

#[allow(dead_code)]
fn assert_process_context_is_generic_over_p<P: Process>(_ctx: &ProcessContext<P>) {
    // Pure type-level check: `ProcessContext` accepts a `P: Process` parameter.
    // If `ProcessContext` ever drops the generic, this signature stops
    // compiling.
}

// ---------------------------------------------------------------------------
// Edge case: the captured proxy outlives the spawning scope.
// ---------------------------------------------------------------------------

/// Given a captured self proxy,
/// when the original `spawn_proxy` is dropped,
/// then the captured proxy still delivers messages — proving the captured
/// proxy's identity is independent of any individual handle's lifetime.
#[tokio::test]
async fn captured_self_proxy_survives_spawn_proxy_drop() {
    // Given
    let system = ProcessSystem::new().await;
    let observed_ctx_pid = Arc::new(Mutex::new(None));
    let captured = Arc::new(Mutex::new(None));
    let inc = Arc::new(AtomicU32::new(0));
    let echo = Arc::new(AtomicU32::new(0));

    let spawn_proxy = system
        .spawn(make_props(
            observed_ctx_pid.clone(),
            Arc::new(Mutex::new(None)),
            captured.clone(),
            inc.clone(),
            echo,
        ))
        .await;
    let _ = wait_for_some(&observed_ctx_pid).await;
    let self_proxy = wait_for_some(&captured).await;
    let observer = spawn_proxy.clone();

    // When: drop the original spawn proxy. The captured proxy must remain
    // alive and continue to address the running actor.
    drop(spawn_proxy);

    self_proxy
        .tell(Increment)
        .await
        .expect("captured self proxy must still deliver after spawn_proxy is dropped");

    let count = observer
        .ask(GetIncrementCount)
        .await
        .expect("ask via independent observer clone must succeed");
    assert_eq!(
        count, 1,
        "actor must still be alive and receive messages through the captured self proxy"
    );
}

// ---------------------------------------------------------------------------
// PID monotonicity sanity check: two spawned processes get different PIDs and
// each ctx.self_proxy() reflects its own process's PID.
// ---------------------------------------------------------------------------

/// Given two independently spawned processes,
/// when each captures `ctx.self_proxy().pid()` inside on_start,
/// then the two captured PIDs differ and each matches its own spawn_pid.
#[tokio::test]
async fn each_processs_self_proxy_reflects_its_own_pid() {
    // Given
    let system = ProcessSystem::new().await;

    let observed_a = Arc::new(Mutex::new(None));
    let proxy_a = system
        .spawn(make_props(
            Arc::new(Mutex::new(None)),
            observed_a.clone(),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
        ))
        .await;
    let pid_a = proxy_a.pid();

    let observed_b = Arc::new(Mutex::new(None));
    let proxy_b = system
        .spawn(make_props(
            Arc::new(Mutex::new(None)),
            observed_b.clone(),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
        ))
        .await;
    let pid_b = proxy_b.pid();

    // When
    let observed_proxy_pid_a = wait_for_some(&observed_a).await;
    let observed_proxy_pid_b = wait_for_some(&observed_b).await;

    // Then
    assert_eq!(
        observed_proxy_pid_a, pid_a,
        "process A's self_proxy must point at process A"
    );
    assert_eq!(
        observed_proxy_pid_b, pid_b,
        "process B's self_proxy must point at process B"
    );
    assert_ne!(
        observed_proxy_pid_a, observed_proxy_pid_b,
        "distinct spawns must produce distinct self_proxy PIDs"
    );
}

// ---------------------------------------------------------------------------
// `self_proxy().pid()` is stable across handler invocations: the same captured
// PID is observed regardless of which message is being processed.
// ---------------------------------------------------------------------------

struct PidProbe;

impl Receive<PidProbe> for SelfAwareProcess {
    type Response = Pid;
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: PidProbe,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<Pid, std::convert::Infallible> {
        Ok(ctx.self_proxy().pid())
    }
}

/// Given a process,
/// when `PidProbe` is asked twice,
/// then both responses return the same PID, equal to the spawn-time PID.
#[tokio::test]
async fn self_proxy_pid_is_stable_across_handler_invocations() {
    // Given
    let system = ProcessSystem::new().await;
    let proxy = system
        .spawn(make_props(
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
        ))
        .await;
    let spawn_pid = proxy.pid();

    // When
    let pid1 = proxy.ask(PidProbe).await.expect("first ask must succeed");
    let pid2 = proxy.ask(PidProbe).await.expect("second ask must succeed");

    // Then
    assert_eq!(pid1, spawn_pid);
    assert_eq!(pid2, spawn_pid);
    assert_eq!(pid1, pid2);
}

// ---------------------------------------------------------------------------
// Helper: avoid unused-warning noise for the AtomicU64 type that callers of
// `wait_for_some` instantiate. Not a behavior test.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _atomic_u64_compile_check() -> AtomicU64 {
    AtomicU64::new(0)
}
