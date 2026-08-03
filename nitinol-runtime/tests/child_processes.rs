//! Integration tests for the parent/child hierarchy feature (Issue #54).
//!
//! These tests pin down:
//! - `PidSet` behaviour observable via `ctx.children()` (len / is_empty /
//!   contains / iter / insertion order)
//! - `ctx.spawn_child` returns a working proxy and registers the child in the
//!   flat `ProcessRegistry`
//! - The parent's `ctx.children()` contains the spawned child Pid
//! - The child's `ctx.parent()` returns the parent Pid; top-level processes
//!   have `parent() == None`
//! - When the parent stops, every child receives `Stop`, their `on_stop` runs,
//!   they are unregistered, and only then does the parent terminate
//! - Multiple children are stopped in **reverse insertion order**
//! - A child that dies naturally (e.g. self-stops) is removed from the
//!   parent's `ctx.children()` via the automatic `Terminated` handler
//! - Three-generation cascading stop: stopping grandparent stops parent and
//!   grandchild
//! - `Poison` still triggers child cleanup (children are stopped + awaited)
//! - `default_idle_timeout` is inherited by children spawned via
//!   `ctx.spawn_child` / `ctx.spawn_child_with_driver`
//!
//! Note: the tests file is intentionally self-contained and does NOT use the
//! shared `common/` helpers — every fixture here exposes child-spawn / parent
//! handles that are specific to the hierarchy API.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, Receive, Terminated, TerminatedReason,
};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Poll an async closure until it returns `true`, panicking after 5s.
///
/// The closure is `FnMut() -> Fut` with `Fut: Future<Output = bool>` — each
/// invocation produces a fresh future. To avoid lifetime tangles, callers
/// should ensure the closure body **owns** everything its future captures
/// (e.g. clone an `Arc` inside the closure body before the `async move`).
async fn wait_until_async<F, Fut>(mut f: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits for an `AtomicBool` flag to become `true`, panicking after 5s.
async fn wait_for_flag(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for flag to become true"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Polls until `received` is `Some`, panicking after 5s.
async fn wait_for_terminated(received: &Arc<Mutex<Option<Terminated>>>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = received.lock().await;
            if guard.is_some() {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Terminated notification"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Tell the watcher to start watching the given PID.
struct WatchPid {
    pid: Pid,
}

/// No-op barrier: when the ask returns, the watcher has processed all prior messages.
struct Barrier;

/// Watcher process that records the first `Terminated` notification it receives.
struct WatcherProcess {
    received: Arc<Mutex<Option<Terminated>>>,
}

impl Process for WatcherProcess {
    fn on_terminated(
        &mut self,
        terminated: Terminated,
        _ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = ()> + Send {
        let received = self.received.clone();
        async move {
            *received.lock().await = Some(terminated);
        }
    }
}

impl Receive<WatchPid> for WatcherProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        msg: WatchPid,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        ctx.watch(msg.pid).await;
        Ok(())
    }
}

impl Receive<Barrier> for WatcherProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Barrier,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

/// Creates `Props<WatcherProcess>` wired to the given shared state.
fn watcher_props(
    received: Arc<Mutex<Option<Terminated>>>,
    idle_timeout: IdleTimeout,
) -> Props<WatcherProcess> {
    let props = Props::new(move || WatcherProcess {
        received: received.clone(),
    });
    let props = props.with_idle_timeout(idle_timeout);
    props
}

// ---------------------------------------------------------------------------
// `ChildProcess`: a minimal child that records its parent Pid and lifecycle
// events.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ChildState {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    /// Pid of the parent observed from inside the child's `on_start`.
    observed_parent: Arc<Mutex<Option<Option<Pid>>>>,
    /// Order index assigned by the parent at spawn time; used to assert
    /// reverse-order stop propagation across multiple children.
    order: u32,
    /// Shared log of `order` values in the order each child reaches `on_stop`.
    stop_order: Arc<Mutex<Vec<u32>>>,
}

struct ChildProcess {
    state: ChildState,
}

impl Process for ChildProcess {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let parent = ctx.parent().copied();
        let state = self.state.clone();
        async move {
            *state.observed_parent.lock().await = Some(parent);
            state.started.store(true, Ordering::SeqCst);
        }
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let state = self.state.clone();
        let order = self.state.order;
        async move {
            state.stop_order.lock().await.push(order);
            state.stopped.store(true, Ordering::SeqCst);
        }
    }
}

fn child_props(state: ChildState) -> Props<ChildProcess> {
    let props = Props::new(move || ChildProcess {
        state: state.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    props
}

/// Request the child to stop itself from within its own handler — used to
/// simulate "natural death" of a child without the parent driving it.
struct StopSelf;

impl Receive<StopSelf> for ChildProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: StopSelf,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let _ = ctx.stop_self().await;
        Ok(())
    }
}

fn fresh_child_state(order: u32, stop_order: Arc<Mutex<Vec<u32>>>) -> ChildState {
    ChildState {
        started: Arc::new(AtomicBool::new(false)),
        stopped: Arc::new(AtomicBool::new(false)),
        observed_parent: Arc::new(Mutex::new(None)),
        order,
        stop_order,
    }
}

// ---------------------------------------------------------------------------
// `ParentProcess`: spawns N children inside `on_start` and exposes its
// `ctx.children()` snapshot and child proxies.
// ---------------------------------------------------------------------------

struct ParentConfig {
    children: Vec<ChildState>,
    /// Snapshot captured from inside parent's `on_start` after spawning the
    /// children. Each tuple is (child_proxy, observed_children_pids).
    captured: Arc<Mutex<Option<ParentSnapshot>>>,
    parent_stopped: Arc<AtomicBool>,
    stop_order: Arc<Mutex<Vec<u32>>>,
}

#[derive(Clone)]
struct ParentSnapshot {
    parent_pid: Pid,
    parent_parent: Option<Pid>,
    child_proxies: Vec<ProcessProxy<ChildProcess>>,
    /// `ctx.children()` snapshot **right after** all children were spawned
    /// (parent's natural-death cleanup has not yet had a chance to run).
    children_after_spawn: Vec<Pid>,
    /// `ctx.children().len()` value right after spawn.
    children_len_after_spawn: usize,
    /// `ctx.children().is_empty()` value before any spawn.
    children_empty_before_spawn: bool,
    /// `ctx.children().contains(<child_pid>)` for each spawned child.
    contains_each_child: Vec<bool>,
}

struct ParentProcess {
    config: ParentConfig,
}

impl Process for ParentProcess {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        // Initial state: no children yet.
        let parent_pid = ctx.pid();
        let parent_parent = ctx.parent().copied();
        let empty_before = ctx.children().is_empty();

        let mut child_proxies = Vec::new();
        let mut contains_each_child = Vec::new();
        for child_state in &self.config.children {
            let proxy = ctx.spawn_child(child_props(child_state.clone())).await;
            let pid = proxy.pid();
            contains_each_child.push(ctx.children().contains(&pid));
            child_proxies.push(proxy);
        }

        let children_after_spawn: Vec<Pid> = ctx.children().iter().copied().collect();
        let children_len_after_spawn = ctx.children().len();

        *self.config.captured.lock().await = Some(ParentSnapshot {
            parent_pid,
            parent_parent,
            child_proxies,
            children_after_spawn,
            children_len_after_spawn,
            children_empty_before_spawn: empty_before,
            contains_each_child,
        });
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let stopped = self.config.parent_stopped.clone();
        let stop_order = self.config.stop_order.clone();
        async move {
            // Parent records itself with sentinel value `u32::MAX` so the
            // test can assert that parent's `on_stop` fires **before** any
            // child's `on_stop`.
            stop_order.lock().await.push(u32::MAX);
            stopped.store(true, Ordering::SeqCst);
        }
    }
}

fn parent_props(config: ParentConfig) -> Props<ParentProcess> {
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let stop_order = config.stop_order.clone();
    let children = config.children.clone();
    let props = Props::new(move || ParentProcess {
        config: ParentConfig {
            children: children.clone(),
            captured: captured.clone(),
            parent_stopped: parent_stopped.clone(),
            stop_order: stop_order.clone(),
        },
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    props
}

// ---------------------------------------------------------------------------
// Top-level checks: PidSet observable behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn top_level_process_has_no_parent_and_empty_children() {
    // Given: a top-level (system-spawned) process
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let config = ParentConfig {
        children: Vec::new(),
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let _proxy = system.spawn(parent_props(config)).await;

    // When: on_start has finished
    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start to complete",
    )
    .await;

    // Then: parent() == None and children() is empty
    let snap = captured.lock().await.clone().unwrap();
    assert!(
        snap.parent_parent.is_none(),
        "top-level process must have ctx.parent() == None"
    );
    assert!(
        snap.children_empty_before_spawn,
        "freshly-spawned process must have empty children()"
    );
    assert_eq!(
        snap.children_len_after_spawn, 0,
        "no children were spawned: len must be 0"
    );
    assert!(snap.children_after_spawn.is_empty());
}

#[tokio::test]
async fn spawn_child_returns_proxy_with_unique_pid_and_registers_in_registry() {
    // Given: a parent that will spawn two children inside on_start
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let c1 = fresh_child_state(1, stop_order.clone());
    let c2 = fresh_child_state(2, stop_order.clone());
    let config = ParentConfig {
        children: vec![c1, c2],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();

    // When: parent is spawned
    let _parent_proxy = system.spawn(parent_props(config)).await;
    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;

    // Then: each child Pid is distinct, and each is registered in the
    // (flat) system registry
    let snap = captured.lock().await.clone().unwrap();
    assert_eq!(snap.child_proxies.len(), 2);
    let pid_a = snap.child_proxies[0].pid();
    let pid_b = snap.child_proxies[1].pid();
    assert_ne!(
        pid_a, pid_b,
        "spawn_child must produce a fresh Pid per child"
    );

    // Registry lookup must find each child (flat registry — PID-keyed,
    // no hierarchy traversal needed).
    assert!(
        system.lookup(pid_a).await.is_some(),
        "child A must be discoverable via the flat ProcessRegistry"
    );
    assert!(
        system.lookup(pid_b).await.is_some(),
        "child B must be discoverable via the flat ProcessRegistry"
    );
}

#[tokio::test]
async fn parent_children_contains_each_spawned_child_in_insertion_order() {
    // Given: a parent that spawns three children A, B, C in this order
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let children_states = vec![
        fresh_child_state(1, stop_order.clone()),
        fresh_child_state(2, stop_order.clone()),
        fresh_child_state(3, stop_order.clone()),
    ];
    let config = ParentConfig {
        children: children_states,
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let _parent_proxy = system.spawn(parent_props(config)).await;

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;

    let snap = captured.lock().await.clone().unwrap();

    // Then: children() length matches the number of spawns
    assert_eq!(
        snap.children_len_after_spawn, 3,
        "children().len() must equal the number of spawn_child calls"
    );

    // Each contains() check returned true at spawn time
    assert!(
        snap.contains_each_child.iter().all(|b| *b),
        "ctx.children().contains(child_pid) must return true immediately \
         after spawn_child returns"
    );

    // Iteration order must match the order of spawn_child calls
    let observed: Vec<Pid> = snap.children_after_spawn.clone();
    let expected: Vec<Pid> = snap.child_proxies.iter().map(|p| p.pid()).collect();
    assert_eq!(
        observed, expected,
        "PidSet must preserve insertion order in iter()"
    );
}

#[tokio::test]
async fn child_parent_returns_spawner_pid() {
    // Given: a parent spawns one child
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let config = ParentConfig {
        children: vec![child_state.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;

    // And: the child has finished its own on_start (which records its
    // observed parent pid)
    wait_for_flag(&child_state.started).await;

    // Then: the child observed Some(parent_pid)
    let observed = *child_state.observed_parent.lock().await;
    assert_eq!(
        observed,
        Some(Some(parent_proxy.pid())),
        "ctx.parent() inside child on_start must equal the spawning parent's Pid"
    );
}

// ---------------------------------------------------------------------------
// Stop propagation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parent_stop_propagates_stop_to_child_and_waits_for_child_on_stop() {
    // Given: a parent with one child
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let config = ParentConfig {
        children: vec![child_state.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;
    let parent_pid = parent_proxy.pid();

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;
    wait_for_flag(&child_state.started).await;
    let child_pid = captured.lock().await.clone().unwrap().child_proxies[0].pid();

    // When: parent is stopped
    parent_proxy.stop().await.expect("parent stop");

    // Then: the child receives Stop, runs on_stop, and is unregistered.
    wait_for_flag(&child_state.stopped).await;
    wait_for_flag(&parent_stopped).await;

    // Stop order (per ProtoActor.go `handleStop` flow encoded by plan D-3):
    //   1. parent.on_stop runs first  → sentinel u32::MAX
    //   2. stopAllChildren sends Stop → child.on_stop runs → records 1
    //   3. parent waits for child Terminated, then exits
    let order = stop_order.lock().await.clone();
    assert_eq!(
        order,
        vec![u32::MAX, 1],
        "parent's on_stop must complete before children are stopped \
         (handleStop flow): the parent sentinel must precede the child's order"
    );

    // The child Pid is no longer in the flat registry — and neither is the parent.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let child_gone = system.lookup(child_pid).await.is_none();
        let parent_gone = system.lookup(parent_pid).await.is_none();
        if child_gone && parent_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for parent+child to unregister"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn parent_stop_propagates_in_reverse_insertion_order_to_multiple_children() {
    // Given: a parent with three children spawned in order 1, 2, 3
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let cs = vec![
        fresh_child_state(1, stop_order.clone()),
        fresh_child_state(2, stop_order.clone()),
        fresh_child_state(3, stop_order.clone()),
    ];
    let stopped_flags: Vec<Arc<AtomicBool>> = cs.iter().map(|s| s.stopped.clone()).collect();

    let config = ParentConfig {
        children: cs,
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;
    // Children may finish on_start asynchronously; wait for all.
    for f in &stopped_flags {
        let _ = f; // touch to silence unused warning; flags polled below
    }

    // When: parent is stopped
    parent_proxy.stop().await.expect("parent stop");
    wait_for_flag(&parent_stopped).await;
    for f in &stopped_flags {
        wait_for_flag(f).await;
    }

    // Then: parent.on_stop runs first (sentinel u32::MAX), then children are
    // stopped in **reverse insertion order** — 3, 2, 1.
    let order = stop_order.lock().await.clone();
    assert_eq!(
        order,
        vec![u32::MAX, 3, 2, 1],
        "stopAllChildren must broadcast Stop in reverse insertion order \
         AFTER the parent's own on_stop completes; expected \
         [parent-sentinel, child-3, child-2, child-1]"
    );
}

#[tokio::test]
async fn natural_child_death_removes_pid_from_parent_children_set() {
    // Given: a parent with one child
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let captured_snapshot = Arc::new(Mutex::new(None));
    let on_terminated_called = Arc::new(AtomicBool::new(false));

    // Query message: ask the parent for its current children set.
    struct QueryChildren;

    struct WatchedParent {
        children_specs: Vec<ChildState>,
        captured: Arc<Mutex<Option<ParentSnapshot>>>,
        on_terminated_called: Arc<AtomicBool>,
    }

    impl Process for WatchedParent {
        async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
            let pid = ctx.pid();
            let mut proxies = Vec::new();
            let mut contains = Vec::new();
            for s in &self.children_specs {
                let p = ctx.spawn_child(child_props(s.clone())).await;
                contains.push(ctx.children().contains(&p.pid()));
                proxies.push(p);
            }
            let snapshot = ParentSnapshot {
                parent_pid: pid,
                parent_parent: ctx.parent().copied(),
                children_after_spawn: ctx.children().iter().copied().collect(),
                children_len_after_spawn: ctx.children().len(),
                children_empty_before_spawn: false,
                contains_each_child: contains,
                child_proxies: proxies,
            };
            *self.captured.lock().await = Some(snapshot);
        }

        fn on_terminated(
            &mut self,
            _terminated: Terminated,
            _ctx: &mut ProcessContext<Self>,
        ) -> impl Future<Output = ()> + Send {
            // After the fix, on_terminated must NOT be called for children
            // that were spawned via spawn_child (without explicit watch).
            let flag = self.on_terminated_called.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Receive<QueryChildren> for WatchedParent {
        type Response = Vec<Pid>;
        type Error = std::convert::Infallible;
        async fn recv(
            &mut self,
            _: QueryChildren,
            ctx: &mut ProcessContext<Self>,
        ) -> Result<Vec<Pid>, std::convert::Infallible> {
            Ok(ctx.children().iter().copied().collect())
        }
    }

    let children_specs = vec![child_state.clone()];
    let captured_clone = captured_snapshot.clone();
    let otc_clone = on_terminated_called.clone();
    let props = Props::new(move || WatchedParent {
        children_specs: children_specs.clone(),
        captured: captured_clone.clone(),
        on_terminated_called: otc_clone.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);

    let parent_proxy = system.spawn(props).await;
    wait_until_async(
        || {
            let captured = captured_snapshot.clone();
            async move { captured.lock().await.is_some() }
        },
        "WatchedParent on_start",
    )
    .await;
    wait_for_flag(&child_state.started).await;

    // When: the child terminates itself
    let snap = captured_snapshot.lock().await.clone().unwrap();
    let child_proxy = snap.child_proxies[0].clone();
    let child_pid = child_proxy.pid();
    child_proxy
        .tell(StopSelf)
        .await
        .expect("self-stop tell must succeed while child is alive");

    // Wait for the child to be removed from the flat registry — this confirms
    // the child has fully terminated and the parent has processed Terminated.
    wait_until_async(
        || {
            let system = &system;
            async move { system.lookup(child_pid).await.is_none() }
        },
        "child to be removed from flat registry",
    )
    .await;

    // Send a QueryChildren barrier to the parent to ensure the Terminated
    // signal has been fully processed by the parent's lifecycle loop.
    let children_after_death = parent_proxy
        .ask(QueryChildren)
        .await
        .expect("QueryChildren must succeed while parent is alive");

    // Then: the child's Pid is no longer in the parent's children set.
    assert!(
        !children_after_death.contains(&child_pid),
        "child Pid must be removed from ctx.children() after natural death; \
         got {:?}",
        children_after_death
    );

    // And: on_terminated must NOT have been called — children spawned via
    // spawn_child (without an explicit ctx.watch call) do not trigger
    // on_terminated (ARCH-REVIEW-001 regression prevention).
    assert!(
        !on_terminated_called.load(Ordering::SeqCst),
        "on_terminated must NOT be called for non-watched children; \
         only explicitly-watched processes trigger on_terminated"
    );
}

// ---------------------------------------------------------------------------
// Three-generation cascading stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_generation_cascading_stop_runs_root_on_stop_first() {
    // Given: GP spawns P; P spawns GC. All three are alive.
    //
    // This nests the same `ParentProcess` fixture: GP's child is also a
    // `ParentProcess`, but to keep the test compact we use a custom process
    // that captures handles to GC.
    let system = ProcessSystem::new().await;

    // Three independent `stopped` flags, one per generation
    let gc_stopped = Arc::new(AtomicBool::new(false));
    let p_stopped = Arc::new(AtomicBool::new(false));
    let gp_stopped = Arc::new(AtomicBool::new(false));

    // Stop order: GC first, then P, then GP
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    // Grandchild
    struct GC {
        stopped: Arc<AtomicBool>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Process for GC {
        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let stopped = self.stopped.clone();
            let order = self.order.clone();
            async move {
                order.lock().await.push("GC");
                stopped.store(true, Ordering::SeqCst);
            }
        }
    }

    fn gc_props(stopped: Arc<AtomicBool>, order: Arc<Mutex<Vec<&'static str>>>) -> Props<GC> {
        let props = Props::new(move || GC {
            stopped: stopped.clone(),
            order: order.clone(),
        });
        let props = props.with_idle_timeout(IdleTimeout::Persistent);
        props
    }

    // Parent (middle generation)
    struct P {
        gc_stopped: Arc<AtomicBool>,
        p_stopped: Arc<AtomicBool>,
        order: Arc<Mutex<Vec<&'static str>>>,
        gc_started: Arc<AtomicBool>,
    }
    impl Process for P {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let gc_stopped = self.gc_stopped.clone();
            let order = self.order.clone();
            let gc_started = self.gc_started.clone();
            async move {
                let _gc_proxy = ctx
                    .spawn_child(gc_props(gc_stopped.clone(), order.clone()))
                    .await;
                gc_started.store(true, Ordering::SeqCst);
            }
        }

        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let stopped = self.p_stopped.clone();
            let order = self.order.clone();
            async move {
                order.lock().await.push("P");
                stopped.store(true, Ordering::SeqCst);
            }
        }
    }

    fn p_props(
        gc_stopped: Arc<AtomicBool>,
        p_stopped: Arc<AtomicBool>,
        order: Arc<Mutex<Vec<&'static str>>>,
        gc_started: Arc<AtomicBool>,
    ) -> Props<P> {
        let props = Props::new(move || P {
            gc_stopped: gc_stopped.clone(),
            p_stopped: p_stopped.clone(),
            order: order.clone(),
            gc_started: gc_started.clone(),
        });
        let props = props.with_idle_timeout(IdleTimeout::Persistent);
        props
    }

    // Grandparent (top of the chain)
    struct GP {
        p_stopped: Arc<AtomicBool>,
        gp_stopped: Arc<AtomicBool>,
        gc_stopped: Arc<AtomicBool>,
        order: Arc<Mutex<Vec<&'static str>>>,
        p_started: Arc<AtomicBool>,
        gc_started: Arc<AtomicBool>,
    }
    impl Process for GP {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let gc_stopped = self.gc_stopped.clone();
            let p_stopped = self.p_stopped.clone();
            let order = self.order.clone();
            let p_started = self.p_started.clone();
            let gc_started = self.gc_started.clone();
            async move {
                let _p_proxy = ctx
                    .spawn_child(p_props(
                        gc_stopped.clone(),
                        p_stopped.clone(),
                        order.clone(),
                        gc_started.clone(),
                    ))
                    .await;
                p_started.store(true, Ordering::SeqCst);
            }
        }

        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let stopped = self.gp_stopped.clone();
            let order = self.order.clone();
            async move {
                order.lock().await.push("GP");
                stopped.store(true, Ordering::SeqCst);
            }
        }
    }

    let p_started = Arc::new(AtomicBool::new(false));
    let gc_started = Arc::new(AtomicBool::new(false));
    let gp_props = Props::new({
        let gc_stopped = gc_stopped.clone();
        let p_stopped = p_stopped.clone();
        let gp_stopped = gp_stopped.clone();
        let order = order.clone();
        let p_started = p_started.clone();
        let gc_started = gc_started.clone();
        move || GP {
            p_stopped: p_stopped.clone(),
            gp_stopped: gp_stopped.clone(),
            gc_stopped: gc_stopped.clone(),
            order: order.clone(),
            p_started: p_started.clone(),
            gc_started: gc_started.clone(),
        }
    });
    let gp_props = gp_props.with_idle_timeout(IdleTimeout::Persistent);

    let gp_proxy = system.spawn(gp_props).await;
    wait_for_flag(&p_started).await;
    wait_for_flag(&gc_started).await;

    // When: GP is stopped
    gp_proxy.stop().await.expect("GP stop");

    // Then: cascading stop runs `on_stop` root-first (per plan D-3 +
    // handleStop flow):
    //   GP.on_stop → GP stops P → P.on_stop → P stops GC → GC.on_stop
    //   (then GC unregisters → P exits → GP exits)
    wait_for_flag(&gc_stopped).await;
    wait_for_flag(&p_stopped).await;
    wait_for_flag(&gp_stopped).await;
    let observed = order.lock().await.clone();
    assert_eq!(
        observed,
        vec!["GP", "P", "GC"],
        "cascading stop must run on_stop root-first because each level \
         executes its own on_stop BEFORE broadcasting Stop to its children"
    );
}

// ---------------------------------------------------------------------------
// Poison
// ---------------------------------------------------------------------------

#[tokio::test]
async fn poison_parent_still_stops_children_and_skips_parent_on_stop() {
    // Given: a parent with one child
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let config = ParentConfig {
        children: vec![child_state.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;
    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;
    wait_for_flag(&child_state.started).await;
    let snap = captured.lock().await.clone().unwrap();
    let parent_pid = snap.parent_pid;
    let child_pid = snap.child_proxies[0].pid();

    // When: parent is poisoned
    parent_proxy.poison().await.expect("parent poison");

    // Then: the child is still stopped (lifecycle propagates Stop to children
    // even when the parent itself is being poisoned — orphans must not
    // survive a poisoned parent)
    wait_for_flag(&child_state.stopped).await;

    // And: child unregisters and so does the parent. Inline polling so the
    // future does NOT borrow from `system` across iterations.
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let child_gone = system.lookup(child_pid).await.is_none();
            let parent_gone = system.lookup(parent_pid).await.is_none();
            if child_gone && parent_gone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for parent+child to unregister after Poison"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // And: the parent's own `on_stop` was NOT called (Poison skips cleanup)
    assert!(
        !parent_stopped.load(Ordering::SeqCst),
        "Poison must skip the parent's on_stop even when children are cleaned up"
    );
}

// ---------------------------------------------------------------------------
// spawn_child_with_driver
// ---------------------------------------------------------------------------

struct TickChild {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    parent_observed: Arc<Mutex<Option<Option<Pid>>>>,
}

impl Process for TickChild {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let started = self.started.clone();
        let parent_observed = self.parent_observed.clone();
        let parent = ctx.parent().copied();
        async move {
            *parent_observed.lock().await = Some(parent);
            started.store(true, Ordering::SeqCst);
        }
    }
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let stopped = self.stopped.clone();
        async move {
            stopped.store(true, Ordering::SeqCst);
        }
    }
}

struct NeverDriver;

impl Driver<TickChild> for NeverDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        std::future::pending()
    }
    async fn apply(
        &mut self,
        _state: &mut TickChild,
        _ctx: &mut ProcessContext<TickChild>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        Ok(())
    }
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn spawn_child_with_driver_propagates_parent_pid_and_stops_with_parent() {
    // Given: a parent that spawns a driver-backed child via
    // `ctx.spawn_child_with_driver`
    let system = ProcessSystem::new().await;
    let child_started = Arc::new(AtomicBool::new(false));
    let child_stopped = Arc::new(AtomicBool::new(false));
    let child_parent_observed: Arc<Mutex<Option<Option<Pid>>>> = Arc::new(Mutex::new(None));
    let parent_stopped = Arc::new(AtomicBool::new(false));

    struct DriverParent {
        child_started: Arc<AtomicBool>,
        child_stopped: Arc<AtomicBool>,
        child_parent_observed: Arc<Mutex<Option<Option<Pid>>>>,
        parent_stopped: Arc<AtomicBool>,
        child_pid: Arc<Mutex<Option<Pid>>>,
    }
    impl Process for DriverParent {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let s = self.child_started.clone();
            let st = self.child_stopped.clone();
            let obs = self.child_parent_observed.clone();
            let child_pid = self.child_pid.clone();
            async move {
                let props = Props::new(move || TickChild {
                    started: s.clone(),
                    stopped: st.clone(),
                    parent_observed: obs.clone(),
                })
                .with_idle_timeout(IdleTimeout::Persistent)
                .with_driver(NeverDriver);
                let proxy = ctx.spawn_child(props).await;
                *child_pid.lock().await = Some(proxy.pid());
            }
        }
        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let p = self.parent_stopped.clone();
            async move {
                p.store(true, Ordering::SeqCst);
            }
        }
    }

    let child_pid_slot = Arc::new(Mutex::new(None::<Pid>));
    let parent_proxy = system
        .spawn({
            let cs = child_started.clone();
            let cst = child_stopped.clone();
            let obs = child_parent_observed.clone();
            let ps = parent_stopped.clone();
            let cps = child_pid_slot.clone();
            let props = Props::new(move || DriverParent {
                child_started: cs.clone(),
                child_stopped: cst.clone(),
                child_parent_observed: obs.clone(),
                parent_stopped: ps.clone(),
                child_pid: cps.clone(),
            });
            let props = props.with_idle_timeout(IdleTimeout::Persistent);
            props
        })
        .await;

    wait_for_flag(&child_started).await;
    let observed = *child_parent_observed.lock().await;
    assert_eq!(
        observed,
        Some(Some(parent_proxy.pid())),
        "driver-backed child's ctx.parent() must equal the spawning parent's Pid"
    );

    // When: the parent is stopped
    parent_proxy.stop().await.expect("parent stop");

    // Then: the driver-backed child is also stopped, parent runs its on_stop
    wait_for_flag(&child_stopped).await;
    wait_for_flag(&parent_stopped).await;
}

// ---------------------------------------------------------------------------
// default_idle_timeout inheritance through spawn_child
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_child_with_inherit_idle_timeout_uses_system_default() {
    // Given: a system with a short default idle timeout
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_millis(50));

    // And: a parent that spawns one child whose Props leaves idle_timeout
    // at the default (Inherit). The child uses no driver opt-out, so the
    // timer will arm.
    struct InheritParent {
        child_started: Arc<AtomicBool>,
        child_stopped: Arc<AtomicBool>,
    }
    impl Process for InheritParent {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let s = self.child_started.clone();
            let st = self.child_stopped.clone();
            async move {
                // No with_idle_timeout call: default is Inherit
                let props = Props::new(move || ChildProcessNoOverride {
                    started: s.clone(),
                    stopped: st.clone(),
                });
                let _proxy = ctx.spawn_child(props).await;
            }
        }
    }

    /// Child variant whose Props does NOT override idle timeout (defaults
    /// to Inherit). On_start records start; on_stop records stop.
    struct ChildProcessNoOverride {
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    }
    impl Process for ChildProcessNoOverride {
        fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            self.started.store(true, Ordering::SeqCst);
            async {}
        }
        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            self.stopped.store(true, Ordering::SeqCst);
            async {}
        }
    }

    let child_started = Arc::new(AtomicBool::new(false));
    let child_stopped = Arc::new(AtomicBool::new(false));

    let parent_props = Props::new({
        let s = child_started.clone();
        let st = child_stopped.clone();
        move || InheritParent {
            child_started: s.clone(),
            child_stopped: st.clone(),
        }
    });
    let parent_props = parent_props.with_idle_timeout(IdleTimeout::Persistent);
    let _parent_proxy = system.spawn(parent_props).await;
    wait_for_flag(&child_started).await;

    // When: no messages are sent (the child's inherited 50ms timer fires)

    // Then: the child stops naturally due to the inherited system default
    wait_for_flag(&child_stopped).await;
}

// ---------------------------------------------------------------------------
// ARCH-REVIEW-001: on_terminated contract for non-watched children
// ---------------------------------------------------------------------------

/// After the ARCH-REVIEW-001 fix, `on_terminated` must NOT be called when a
/// child spawned via `spawn_child` dies naturally (without an explicit
/// `ctx.watch` call on the child).  The Terminated signal is still used
/// internally to remove the child from `ctx.children()`, but it does not
/// reach user code.
#[tokio::test]
async fn child_natural_death_does_not_invoke_on_terminated() {
    let system = ProcessSystem::new().await;
    let on_terminated_called = Arc::new(AtomicBool::new(false));
    let child_started = Arc::new(AtomicBool::new(false));
    let child_pid_slot: Arc<Mutex<Option<Pid>>> = Arc::new(Mutex::new(None));
    let child_proxy_slot: Arc<Mutex<Option<ProcessProxy<ChildProcess>>>> =
        Arc::new(Mutex::new(None));

    // Barrier: when ask returns, all prior messages (incl. Terminated) have
    // been processed by the parent's loop.
    struct BarrierMsg;

    struct NonWatchingParent {
        on_terminated_called: Arc<AtomicBool>,
        child_started: Arc<AtomicBool>,
        child_pid_slot: Arc<Mutex<Option<Pid>>>,
        child_proxy_slot: Arc<Mutex<Option<ProcessProxy<ChildProcess>>>>,
    }

    impl Process for NonWatchingParent {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let cs = self.child_started.clone();
            let pid_slot = self.child_pid_slot.clone();
            let proxy_slot = self.child_proxy_slot.clone();
            async move {
                let stop_order = Arc::new(Mutex::new(Vec::new()));
                let child_state = ChildState {
                    started: cs.clone(),
                    stopped: Arc::new(AtomicBool::new(false)),
                    observed_parent: Arc::new(Mutex::new(None)),
                    order: 1,
                    stop_order,
                };
                // Spawn child WITHOUT calling ctx.watch — no explicit watch.
                let proxy = ctx.spawn_child(child_props(child_state)).await;
                *pid_slot.lock().await = Some(proxy.pid());
                *proxy_slot.lock().await = Some(proxy);
            }
        }

        fn on_terminated(
            &mut self,
            _terminated: Terminated,
            _ctx: &mut ProcessContext<Self>,
        ) -> impl Future<Output = ()> + Send {
            // Must NOT be called for non-watched children.
            let flag = self.on_terminated_called.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Receive<BarrierMsg> for NonWatchingParent {
        type Response = ();
        type Error = std::convert::Infallible;
        async fn recv(
            &mut self,
            _: BarrierMsg,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), std::convert::Infallible> {
            Ok(())
        }
    }

    let otc = on_terminated_called.clone();
    let cs = child_started.clone();
    let pid_slot = child_pid_slot.clone();
    let proxy_slot = child_proxy_slot.clone();
    let props = Props::new(move || NonWatchingParent {
        on_terminated_called: otc.clone(),
        child_started: cs.clone(),
        child_pid_slot: pid_slot.clone(),
        child_proxy_slot: proxy_slot.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    let parent_proxy = system.spawn(props).await;
    wait_for_flag(&child_started).await;

    // When: the child dies naturally (no explicit ctx.watch call).
    let child_proxy = child_proxy_slot.lock().await.clone().unwrap();
    let child_pid = child_pid_slot.lock().await.unwrap();
    child_proxy.tell(StopSelf).await.expect("child stop_self");

    // Wait for the child to be removed from the registry (it has terminated).
    wait_until_async(
        || {
            let system = &system;
            async move { system.lookup(child_pid).await.is_none() }
        },
        "child to be removed from flat registry",
    )
    .await;

    // Send a barrier ask so the parent processes any pending signals before
    // we check the flag.
    parent_proxy
        .ask(BarrierMsg)
        .await
        .expect("barrier ask must succeed while parent is alive");

    // Then: on_terminated was NOT called for the non-watched child.
    assert!(
        !on_terminated_called.load(Ordering::SeqCst),
        "on_terminated must NOT be called for a child that was spawned via \
         spawn_child without an explicit ctx.watch call (ARCH-REVIEW-001)"
    );
}

/// Positive case for ARCH-REVIEW-001: when the parent explicitly calls
/// `ctx.watch(child_pid)` after spawning the child, `on_terminated` IS
/// called when the child dies naturally.
#[tokio::test]
async fn explicitly_watched_child_on_terminated_is_called() {
    let system = ProcessSystem::new().await;
    let on_terminated_called = Arc::new(AtomicBool::new(false));
    let on_terminated_who: Arc<Mutex<Option<Pid>>> = Arc::new(Mutex::new(None));
    let child_started = Arc::new(AtomicBool::new(false));
    let child_pid_slot: Arc<Mutex<Option<Pid>>> = Arc::new(Mutex::new(None));
    let child_proxy_slot: Arc<Mutex<Option<ProcessProxy<ChildProcess>>>> =
        Arc::new(Mutex::new(None));

    struct WatchingParent {
        on_terminated_called: Arc<AtomicBool>,
        on_terminated_who: Arc<Mutex<Option<Pid>>>,
        child_started: Arc<AtomicBool>,
        child_pid_slot: Arc<Mutex<Option<Pid>>>,
        child_proxy_slot: Arc<Mutex<Option<ProcessProxy<ChildProcess>>>>,
    }

    impl Process for WatchingParent {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let cs = self.child_started.clone();
            let pid_slot = self.child_pid_slot.clone();
            let proxy_slot = self.child_proxy_slot.clone();
            async move {
                let stop_order = Arc::new(Mutex::new(Vec::new()));
                let child_state = ChildState {
                    started: cs.clone(),
                    stopped: Arc::new(AtomicBool::new(false)),
                    observed_parent: Arc::new(Mutex::new(None)),
                    order: 1,
                    stop_order,
                };
                let proxy = ctx.spawn_child(child_props(child_state)).await;
                let child_pid = proxy.pid();
                // Explicitly watch the child — this is the key difference from
                // the non-watched case.
                ctx.watch(child_pid).await;
                *pid_slot.lock().await = Some(child_pid);
                *proxy_slot.lock().await = Some(proxy);
            }
        }

        fn on_terminated(
            &mut self,
            terminated: Terminated,
            _ctx: &mut ProcessContext<Self>,
        ) -> impl Future<Output = ()> + Send {
            // MUST be called because we explicitly called ctx.watch(child_pid).
            let flag = self.on_terminated_called.clone();
            let who_slot = self.on_terminated_who.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
                *who_slot.lock().await = Some(terminated.who);
            }
        }
    }

    let otc = on_terminated_called.clone();
    let otw = on_terminated_who.clone();
    let cs = child_started.clone();
    let pid_slot = child_pid_slot.clone();
    let proxy_slot = child_proxy_slot.clone();
    let props = Props::new(move || WatchingParent {
        on_terminated_called: otc.clone(),
        on_terminated_who: otw.clone(),
        child_started: cs.clone(),
        child_pid_slot: pid_slot.clone(),
        child_proxy_slot: proxy_slot.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    let _parent_proxy = system.spawn(props).await;
    wait_for_flag(&child_started).await;

    // When: the child dies naturally (parent had explicitly called ctx.watch).
    let child_proxy = child_proxy_slot.lock().await.clone().unwrap();
    let child_pid = child_pid_slot.lock().await.unwrap();
    child_proxy.tell(StopSelf).await.expect("child stop_self");

    // Then: on_terminated IS called with the child's Pid.
    wait_for_flag(&on_terminated_called).await;

    let observed_who = on_terminated_who.lock().await;
    assert_eq!(
        *observed_who,
        Some(child_pid),
        "on_terminated must be called with the explicitly-watched child's Pid"
    );
}

// ---------------------------------------------------------------------------
// Integration: external watcher still receives Terminated for the parent
// after children have all been stopped.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn external_watcher_receives_parent_terminated_after_children_drained() {
    // Given: a parent with one child + an external watcher of the parent
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let config = ParentConfig {
        children: vec![child_state.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;
    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;
    wait_for_flag(&child_state.started).await;
    let parent_pid = parent_proxy.pid();

    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system
        .spawn(watcher_props(received.clone(), IdleTimeout::Persistent))
        .await;
    watcher_proxy
        .tell(WatchPid { pid: parent_pid })
        .await
        .expect("watch parent must succeed");
    watcher_proxy.ask(Barrier).await.expect("watcher barrier");

    // When: parent is stopped
    parent_proxy.stop().await.expect("parent stop");

    // Then: the external watcher eventually receives Terminated{who=parent_pid,
    // why=Stopped} (children-first cleanup must not block the final watcher
    // notification path)
    wait_for_terminated(&received).await;
    let t = received.lock().await.clone().unwrap();
    assert_eq!(t.who, parent_pid);
    assert_eq!(t.why, TerminatedReason::Stopped);

    // And: parent's on_stop ran BEFORE the child was stopped, then child's
    // on_stop ran, then parent waited and finally emitted Terminated to the
    // watcher.
    let order = stop_order.lock().await.clone();
    assert_eq!(
        order.first().copied(),
        Some(u32::MAX),
        "parent on_stop sentinel must be the first entry (pre-stop cleanup \
         runs before children are stopped)"
    );
    assert_eq!(
        order.last().copied(),
        Some(1),
        "child on_stop must be the last entry (children are drained after \
         parent on_stop completes; the parent does not run on_stop again \
         after the wait)"
    );
}

// ---------------------------------------------------------------------------
// `ctx.children()` is empty immediately after spawn_child on a process whose
// child has already self-stopped before the parent advances.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_dying_before_parent_stop_decrements_children_len() {
    // Given: a parent with two children, then it tells one to stop_self
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let alive_state = fresh_child_state(2, stop_order.clone());
    let dying_state = fresh_child_state(1, stop_order.clone());
    let config = ParentConfig {
        children: vec![dying_state.clone(), alive_state.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;
    wait_for_flag(&dying_state.started).await;
    wait_for_flag(&alive_state.started).await;

    let snap = captured.lock().await.clone().unwrap();
    let dying_proxy = snap.child_proxies[0].clone();

    // When: one child dies first, then the parent is stopped
    dying_proxy.tell(StopSelf).await.expect("dying child");
    wait_for_flag(&dying_state.stopped).await;

    parent_proxy.stop().await.expect("parent stop");
    wait_for_flag(&parent_stopped).await;

    // Then: the parent did NOT block waiting for the already-dead child;
    // The sequence is:
    //   1. dying child self-stop → records 1 (natural death)
    //   2. parent receives Stop, runs on_stop → records u32::MAX
    //   3. parent stops the surviving child → records 2
    //   4. parent waits for child 2 Terminated, then exits
    let order = stop_order.lock().await.clone();
    assert_eq!(
        order,
        vec![1, u32::MAX, 2],
        "stop_order: dying child's natural on_stop (1) → parent's pre-stop \
         on_stop sentinel (u32::MAX) → surviving child's stop (2). \
         The parent must not deadlock waiting on the already-dead child."
    );
}

// ---------------------------------------------------------------------------
// Regression: reverse insertion order is preserved after a mid-set child dies
// naturally (family_tag: spec-noncompliance, ARCH-REVIEW-013 /
// VAL-NEW-reverse-stop-after-removal-L312)
// ---------------------------------------------------------------------------

/// Regression: when a middle child (c2) dies naturally before the parent stops,
/// the remaining live children (c1 and c3) must still be stopped in reverse
/// **insertion** order — c3 first, then c1.
///
/// Before the tombstone fix, `PidSet::remove` used swap-remove, which moved c3
/// into c2's slot, making the internal order [c1, c3].  `iter_rev()` then
/// yielded [c3, c1] — which accidentally matched the expected result for this
/// 3-child case.  The true regression manifests with ≥4 children where the
/// slot shuffle corrupts the order.  This test uses 4 children so the bug is
/// observable: c4 fills c2's slot → Vec = [c1, c4, c3], reversed = [c3, c4,
/// c1], but the correct reverse insertion order is [c4, c3, c1].
#[tokio::test]
async fn remaining_children_stopped_in_reverse_insertion_order_after_middle_child_dies() {
    // Spawn order: c1(1), c2(2), c3(3), c4(4).  c2 dies naturally first.
    // Expected stop_order after parent stop:
    //   2 (c2 natural death) → u32::MAX (parent on_stop) → 4, 3, 1 (remaining in reverse).
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let c1 = fresh_child_state(1, stop_order.clone());
    let c2 = fresh_child_state(2, stop_order.clone());
    let c3 = fresh_child_state(3, stop_order.clone());
    let c4 = fresh_child_state(4, stop_order.clone());

    let config = ParentConfig {
        children: vec![c1.clone(), c2.clone(), c3.clone(), c4.clone()],
        captured: Arc::new(Mutex::new(None)),
        parent_stopped: Arc::new(AtomicBool::new(false)),
        stop_order: stop_order.clone(),
    };
    let captured = config.captured.clone();
    let parent_stopped = config.parent_stopped.clone();
    let parent_proxy = system.spawn(parent_props(config)).await;

    wait_until_async(
        || {
            let captured = captured.clone();
            async move { captured.lock().await.is_some() }
        },
        "parent on_start",
    )
    .await;

    // Wait for all children to start before issuing StopSelf to c2.
    wait_for_flag(&c1.started).await;
    wait_for_flag(&c2.started).await;
    wait_for_flag(&c3.started).await;
    wait_for_flag(&c4.started).await;

    // c2 dies naturally (self-initiated stop).
    let snap = captured.lock().await.clone().unwrap();
    let c2_proxy = snap.child_proxies[1].clone();
    c2_proxy.tell(StopSelf).await.expect("c2 stop_self");
    wait_for_flag(&c2.stopped).await;

    // Now stop the parent — remaining children must stop in reverse insertion
    // order: c4 (inserted last) → c3 → c1.
    parent_proxy.stop().await.expect("parent stop");
    wait_for_flag(&parent_stopped).await;
    wait_for_flag(&c1.stopped).await;
    wait_for_flag(&c3.stopped).await;
    wait_for_flag(&c4.stopped).await;

    let order = stop_order.lock().await.clone();
    assert_eq!(
        order,
        vec![2, u32::MAX, 4, 3, 1],
        "After c2 dies naturally, remaining children must stop in reverse \
         insertion order (c4 → c3 → c1). Got: {order:?}"
    );
}

// ---------------------------------------------------------------------------
// Restart supervision: old children are stopped before new on_start runs
// ---------------------------------------------------------------------------

/// A child that does nothing — used by the restart regression test.
struct NoOpChild;
impl Process for NoOpChild {}

/// Trigger a handler error to provoke a restart.
struct TriggerRestart;

struct RestartableWithChild {
    /// Pid recorded by each `on_start` invocation (appended across restarts).
    child_pids: Arc<Mutex<Vec<Pid>>>,
    /// `ctx.children()` snapshot captured at the **beginning** of each
    /// `on_start`, before any new child is spawned.
    children_before_spawn: Arc<Mutex<Vec<Vec<Pid>>>>,
}

impl Process for RestartableWithChild {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let pids = self.child_pids.clone();
        let cbs = self.children_before_spawn.clone();
        async move {
            let snapshot: Vec<Pid> = ctx.children().iter().copied().collect();
            cbs.lock().await.push(snapshot);

            let p = Props::new(move || NoOpChild);
            let p = p.with_idle_timeout(IdleTimeout::Persistent);
            let proxy = ctx.spawn_child(p).await;
            pids.lock().await.push(proxy.pid());
        }
    }
}

impl Receive<TriggerRestart> for RestartableWithChild {
    type Response = ();
    type Error = HandlerError;
    async fn recv(
        &mut self,
        _: TriggerRestart,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), HandlerError> {
        Err(HandlerError)
    }
}

#[tokio::test]
async fn restart_stops_old_children_before_on_start_reruns() {
    // Given: a process with Restart supervision that spawns one child per on_start
    let system = ProcessSystem::new().await;
    let child_pids: Arc<Mutex<Vec<Pid>>> = Arc::new(Mutex::new(Vec::new()));
    let children_before_spawn: Arc<Mutex<Vec<Vec<Pid>>>> = Arc::new(Mutex::new(Vec::new()));

    let pids_clone = child_pids.clone();
    let cbs_clone = children_before_spawn.clone();
    let props = Props::new(move || RestartableWithChild {
        child_pids: pids_clone.clone(),
        children_before_spawn: cbs_clone.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));

    let proxy = system.spawn(props).await;

    // Wait for first on_start to complete
    wait_until_async(
        || {
            let pids = child_pids.clone();
            async move { !pids.lock().await.is_empty() }
        },
        "first on_start to complete",
    )
    .await;

    let first_child_pid = child_pids.lock().await[0];
    assert!(
        system.lookup(first_child_pid).await.is_some(),
        "first child must be registered after initial on_start"
    );

    // When: trigger a restart via an error-returning handler
    proxy
        .tell(TriggerRestart)
        .await
        .expect("tell TriggerRestart must succeed while process is alive");

    // Then: wait for the second on_start to complete (two child Pids recorded)
    wait_until_async(
        || {
            let pids = child_pids.clone();
            async move { pids.lock().await.len() >= 2 }
        },
        "second on_start to complete after restart",
    )
    .await;

    let second_child_pid = child_pids.lock().await[1];

    // The old child must be unregistered
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if system.lookup(first_child_pid).await.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for old child to be unregistered after restart"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The new child must be registered
    assert!(
        system.lookup(second_child_pid).await.is_some(),
        "new child after restart must be in registry"
    );

    // The second on_start must have observed an EMPTY ctx.children() before
    // spawning — proving that the runtime stopped the old child before
    // rerunning on_start.
    let snapshots = children_before_spawn.lock().await.clone();
    assert!(
        snapshots.len() >= 2,
        "children_before_spawn must have at least two entries (one per on_start)"
    );
    assert!(
        snapshots[1].is_empty(),
        "ctx.children() at the start of the second on_start must be empty; \
         old children must be stopped before restart: got {:?}",
        snapshots[1]
    );
}

// ---------------------------------------------------------------------------
// Restart supervision: parent on_stop fires before child on_stop
// (AI-REVIEW-007 regression test)
// ---------------------------------------------------------------------------

/// Process under Restart supervision that:
/// - Spawns one `ChildProcess` per `on_start`.
/// - Records its own `on_stop` as `u32::MAX` in `stop_order`.
/// - Exposes an `on_start_count` counter to detect when the second start runs.
struct RestartableTracksStopOrder {
    stop_order: Arc<Mutex<Vec<u32>>>,
    child_state: ChildState,
    on_start_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Process for RestartableTracksStopOrder {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let child_state = self.child_state.clone();
        let count = self.on_start_count.clone();
        async move {
            ctx.spawn_child(child_props(child_state)).await;
            count.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let stop_order = self.stop_order.clone();
        async move {
            stop_order.lock().await.push(u32::MAX);
        }
    }
}

impl Receive<TriggerRestart> for RestartableTracksStopOrder {
    type Response = ();
    type Error = HandlerError;
    async fn recv(
        &mut self,
        _: TriggerRestart,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), HandlerError> {
        Err(HandlerError)
    }
}

// ---------------------------------------------------------------------------
// Regression AI-REVIEW-010: ctx.unwatch(child) must not break cascade-stop
// ---------------------------------------------------------------------------

/// Regression for AI-REVIEW-010: a parent that calls `ctx.unwatch(child_pid)`
/// after spawning a child must still be able to stop cleanly.  Before the fix,
/// `unwatch` removed the parent from the child's watcher set, so the child
/// would never send `Terminated` back to the parent.  `stop_children_and_wait`
/// would then block forever (deadlock).
#[tokio::test]
async fn unwatch_child_does_not_prevent_parent_stop_completion() {
    let system = ProcessSystem::new().await;
    let parent_stopped = Arc::new(AtomicBool::new(false));
    let child_pid_slot: Arc<Mutex<Option<Pid>>> = Arc::new(Mutex::new(None));
    let on_start_done = Arc::new(AtomicBool::new(false));

    struct UnwatchingParent {
        parent_stopped: Arc<AtomicBool>,
        child_pid: Arc<Mutex<Option<Pid>>>,
        on_start_done: Arc<AtomicBool>,
    }

    impl Process for UnwatchingParent {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let child_pid_slot = self.child_pid.clone();
            let on_start_done = self.on_start_done.clone();
            async move {
                let stop_order = Arc::new(Mutex::new(Vec::new()));
                let proxy = ctx
                    .spawn_child(child_props(fresh_child_state(1, stop_order)))
                    .await;
                let child_pid = proxy.pid();
                *child_pid_slot.lock().await = Some(child_pid);
                // Deliberately unwatch the child.  Without the fix the parent
                // would deadlock in stop_children_and_wait because the child
                // no longer sends Terminated back to the parent.
                ctx.unwatch(child_pid).await;
                on_start_done.store(true, Ordering::SeqCst);
            }
        }

        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let f = self.parent_stopped.clone();
            async move {
                f.store(true, Ordering::SeqCst);
            }
        }
    }

    let ps = parent_stopped.clone();
    let cps = child_pid_slot.clone();
    let osd = on_start_done.clone();
    let props = Props::new(move || UnwatchingParent {
        parent_stopped: ps.clone(),
        child_pid: cps.clone(),
        on_start_done: osd.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    let parent_proxy = system.spawn(props).await;
    let parent_pid = parent_proxy.pid();

    // Wait until on_start has spawned the child and called unwatch.
    wait_for_flag(&on_start_done).await;
    let child_pid = child_pid_slot.lock().await.unwrap();

    // When: the parent is stopped.
    parent_proxy.stop().await.expect("parent stop");

    // Then: parent's on_stop runs and both processes unregister — no deadlock.
    wait_for_flag(&parent_stopped).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let child_gone = system.lookup(child_pid).await.is_none();
        let parent_gone = system.lookup(parent_pid).await.is_none();
        if child_gone && parent_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for parent and child to unregister: \
             unwatch must not prevent the cascade-stop from completing"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Regression (AI-REVIEW-007): during a supervision restart, the parent's
/// `on_stop` must fire before any child's `on_stop`, matching the spec order
/// (`on_stop` → stop children → await `Terminated`).
#[tokio::test]
async fn restart_on_stop_fires_before_child_on_stop() {
    let system = ProcessSystem::new().await;
    let stop_order = Arc::new(Mutex::new(Vec::new()));
    let child_state = fresh_child_state(1, stop_order.clone());
    let on_start_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let props = Props::new({
        let so = stop_order.clone();
        let cs = child_state.clone();
        let count = on_start_count.clone();
        move || RestartableTracksStopOrder {
            stop_order: so.clone(),
            child_state: cs.clone(),
            on_start_count: count.clone(),
        }
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));

    let proxy = system.spawn(props).await;

    // Wait for the first on_start to complete (child spawned).
    wait_until_async(
        || {
            let count = on_start_count.clone();
            async move { count.load(Ordering::SeqCst) >= 1 }
        },
        "first on_start to complete",
    )
    .await;
    // Wait for the first child to start so we know a child exists to stop.
    wait_for_flag(&child_state.started).await;

    // Trigger a restart via an error-returning handler.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("tell TriggerRestart must succeed while process is alive");

    // Wait for the second on_start to complete — the restart is fully done.
    wait_until_async(
        || {
            let count = on_start_count.clone();
            async move { count.load(Ordering::SeqCst) >= 2 }
        },
        "second on_start to complete after restart",
    )
    .await;

    // Parent's on_stop (u32::MAX) must precede child's on_stop (1).
    let order = stop_order.lock().await.clone();
    assert!(
        order.len() >= 2,
        "at least two on_stop calls must have been recorded during restart; got {order:?}"
    );
    assert_eq!(
        order[0],
        u32::MAX,
        "parent's on_stop must fire before child's on_stop during restart; got {order:?}"
    );
    assert_eq!(
        order[1], 1,
        "child's on_stop must fire after parent's on_stop during restart; got {order:?}"
    );
}

// ---------------------------------------------------------------------------
// Restart supervision: signals received during child-drain are not lost
// (AI-REVIEW-003 regression tests)
// ---------------------------------------------------------------------------

/// A child that blocks in `on_stop` until `stop_gate` is notified.
/// `stop_started` is set to `true` when `on_stop` first runs so the test
/// can detect when the child-drain phase is in progress.
struct StallingChild {
    stop_gate: Arc<Notify>,
    stop_started: Arc<AtomicBool>,
}

impl Process for StallingChild {
    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let gate = self.stop_gate.clone();
        let started = self.stop_started.clone();
        async move {
            started.store(true, Ordering::SeqCst);
            gate.notified().await;
        }
    }
}

/// Process under Restart supervision that spawns a `StallingChild` on every
/// `on_start`. Records whether its own `on_stop` was ever called.
struct PoisonableRestarter {
    stop_gate: Arc<Notify>,
    child_stop_started: Arc<AtomicBool>,
    on_stop_called: Arc<AtomicBool>,
}

impl Process for PoisonableRestarter {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let gate = self.stop_gate.clone();
        let started = self.child_stop_started.clone();
        async move {
            let p = Props::new(move || StallingChild {
                stop_gate: gate.clone(),
                stop_started: started.clone(),
            });
            let p = p.with_idle_timeout(IdleTimeout::Persistent);
            ctx.spawn_child(p).await;
        }
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let flag = self.on_stop_called.clone();
        async move {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

impl Receive<TriggerRestart> for PoisonableRestarter {
    type Response = ();
    type Error = HandlerError;
    async fn recv(
        &mut self,
        _: TriggerRestart,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), HandlerError> {
        Err(HandlerError)
    }
}

/// Regression: a `Poison` signal sent to a restarting process while it is
/// waiting in the child-drain phase must abort the restart and cause the
/// process to exit with `Poisoned` reason. Before the fix, the `Poison`
/// signal was silently dropped and the process restarted normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poison_during_restart_aborts_restart_and_exits_poisoned() {
    let system = ProcessSystem::new().await;

    let stop_gate = Arc::new(Notify::new());
    let child_stop_started = Arc::new(AtomicBool::new(false));
    let on_stop_called = Arc::new(AtomicBool::new(false));

    // Watcher observes the final Terminated reason of the parent.
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher = system
        .spawn(watcher_props(received.clone(), IdleTimeout::Persistent))
        .await;

    let props = Props::new({
        let gate = stop_gate.clone();
        let started = child_stop_started.clone();
        let on_stop = on_stop_called.clone();
        move || PoisonableRestarter {
            stop_gate: gate.clone(),
            child_stop_started: started.clone(),
            on_stop_called: on_stop.clone(),
        }
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));

    let proxy = system.spawn(props).await;

    // Establish a watch before triggering the restart so we capture the
    // final Terminated signal.
    watcher
        .tell(WatchPid { pid: proxy.pid() })
        .await
        .expect("watch must succeed");
    watcher.ask(Barrier).await.expect("barrier");

    // Trigger a restart via an error-returning handler.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("tell TriggerRestart must succeed while process is alive");

    // Wait until the stalling child's on_stop has started — at this point the
    // parent is blocked inside stop_children_for_restart, waiting for the
    // child's Terminated.
    wait_for_flag(&child_stop_started).await;

    // Inject Poison while the parent is mid-restart.
    proxy.poison().await.expect("poison must succeed");

    // Allow the stalling child to finish, which unblocks the parent.
    stop_gate.notify_one();

    // The parent must exit with Poisoned (not restart again).
    wait_for_terminated(&received).await;
    let t = received.lock().await.clone().unwrap();
    assert_eq!(
        t.why,
        TerminatedReason::Poisoned,
        "Poison during restart's child-drain must abort restart and exit with Poisoned"
    );

    // on_stop must have been called — it runs before children are stopped,
    // regardless of whether a Poison arrives during the child-drain wait.
    assert!(
        on_stop_called.load(Ordering::SeqCst),
        "Poison during restart must invoke on_stop for the old state before child-drain"
    );
}

/// Process under Restart supervision that:
/// - On the first start: watches `watch_pid` and spawns a `StallingChild`.
/// - On subsequent starts: only spawns a new `StallingChild` (no re-watch).
/// - Records all Terminated notifications received.
struct RestartableWatcher {
    /// External Pid to watch on the first start. None on subsequent starts.
    watch_target: Option<Pid>,
    stop_gate: Arc<Notify>,
    child_stop_started: Arc<AtomicBool>,
    /// Pid + reason of every Terminated notification received via on_terminated.
    terminated_events: Arc<Mutex<Vec<(Pid, TerminatedReason)>>>,
    /// Shared across restarts: set to true after the first on_start, so
    /// subsequent restarts skip the ctx.watch call.
    first_start_done: Arc<AtomicBool>,
}

impl Process for RestartableWatcher {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        let watch_target = self.watch_target;
        let gate = self.stop_gate.clone();
        let started = self.child_stop_started.clone();
        let first_start_done = self.first_start_done.clone();
        async move {
            if !first_start_done.swap(true, Ordering::SeqCst) {
                // First start only: set up the death-watch on the external process.
                if let Some(pid) = watch_target {
                    ctx.watch(pid).await;
                }
            }
            let p = Props::new(move || StallingChild {
                stop_gate: gate.clone(),
                stop_started: started.clone(),
            });
            let p = p.with_idle_timeout(IdleTimeout::Persistent);
            ctx.spawn_child(p).await;
        }
    }

    fn on_terminated(
        &mut self,
        terminated: Terminated,
        _ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = ()> + Send {
        let events = self.terminated_events.clone();
        async move {
            events.lock().await.push((terminated.who, terminated.why));
        }
    }
}

impl Receive<TriggerRestart> for RestartableWatcher {
    type Response = ();
    type Error = HandlerError;
    async fn recv(
        &mut self,
        _: TriggerRestart,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), HandlerError> {
        Err(HandlerError)
    }
}

/// Regression: a non-child `Terminated` signal (death-watch) arriving while a
/// restarting process is waiting for its children to stop must be replayed to
/// the new process state after the restart completes. Before the fix the signal
/// was silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_child_terminated_during_restart_replayed_to_new_state() {
    let system = ProcessSystem::new().await;

    // Spawn the external process W that P will watch.
    let w_props = Props::new(|| NoOpChild);
    let w_props = w_props.with_idle_timeout(IdleTimeout::Persistent);
    let w_proxy = system.spawn(w_props).await;
    let w_pid = w_proxy.pid();

    let stop_gate = Arc::new(Notify::new());
    let child_stop_started = Arc::new(AtomicBool::new(false));
    let terminated_events: Arc<Mutex<Vec<(Pid, TerminatedReason)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let first_start_done = Arc::new(AtomicBool::new(false));

    let props = Props::new({
        let gate = stop_gate.clone();
        let started = child_stop_started.clone();
        let events = terminated_events.clone();
        let fsd = first_start_done.clone();
        move || RestartableWatcher {
            watch_target: Some(w_pid),
            stop_gate: gate.clone(),
            child_stop_started: started.clone(),
            terminated_events: events.clone(),
            first_start_done: fsd.clone(),
        }
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));

    let proxy = system.spawn(props).await;

    // Wait for the first on_start to complete (watch + child spawned).
    // We detect this by waiting until the child_stop_started flag is NOT yet
    // true and the first_start_done flag IS true.
    wait_until_async(
        || {
            let fsd = first_start_done.clone();
            async move { fsd.load(Ordering::SeqCst) }
        },
        "first on_start to complete",
    )
    .await;

    // Trigger a restart.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("tell TriggerRestart must succeed while process is alive");

    // Wait for the stalling child's on_stop to start — the parent is now
    // blocked in stop_children_for_restart.
    wait_for_flag(&child_stop_started).await;

    // Stop W while the parent is in the child-drain phase. This produces a
    // Terminated signal in the parent's sys_rx that must be buffered and
    // replayed (not dropped).
    w_proxy
        .stop()
        .await
        .expect("stop W must succeed while W is alive");

    // Allow the stalling child to finish, which unblocks the parent and
    // completes the restart.
    stop_gate.notify_one();

    // Wait for the new state to receive the deferred Terminated for W.
    wait_until_async(
        || {
            let events = terminated_events.clone();
            async move {
                events
                    .lock()
                    .await
                    .iter()
                    .any(|(pid, why)| *pid == w_pid && *why == TerminatedReason::Stopped)
            }
        },
        "new state on_terminated to receive W's Stopped notification",
    )
    .await;
}

// ---------------------------------------------------------------------------
// ARCH-REVIEW-005 regression: no-watch child termination during restart
// must NOT be replayed to the new state.
// ---------------------------------------------------------------------------

/// Regression for ARCH-REVIEW-005 (design-violation): a non-watched child's
/// termination during a supervision restart must NOT be surfaced as
/// `on_terminated` to the new process state.
///
/// Before the fix, duplicate `is_child / is_watched` logic in
/// `stop_children_for_restart` incorrectly buffered non-watched child stops
/// into `pending_terminated`.  With the fix, `stop_children_for_restart`
/// checks `ctx.explicit_watches` — hierarchy-only children (no explicit watch)
/// are removed from `ctx.children` but never placed in `pending_terminated`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_watched_child_terminated_during_restart_not_replayed() {
    let system = ProcessSystem::new().await;
    let stop_gate = Arc::new(Notify::new());
    let child_stop_started = Arc::new(AtomicBool::new(false));
    let first_start_done = Arc::new(AtomicBool::new(false));
    let second_start_done = Arc::new(AtomicBool::new(false));
    let on_terminated_called = Arc::new(AtomicBool::new(false));

    struct RestartableNoWatch {
        stop_gate: Arc<Notify>,
        child_stop_started: Arc<AtomicBool>,
        first_start_done: Arc<AtomicBool>,
        second_start_done: Arc<AtomicBool>,
        on_terminated_called: Arc<AtomicBool>,
    }

    struct Barrier;

    impl Process for RestartableNoWatch {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let gate = self.stop_gate.clone();
            let started = self.child_stop_started.clone();
            let first = self.first_start_done.clone();
            let second = self.second_start_done.clone();
            async move {
                if !first.swap(true, Ordering::SeqCst) {
                    // First start — reset child_stop_started and spawn a stalling child.
                    started.store(false, Ordering::SeqCst);
                } else {
                    // Second start (post-restart) — signal so the test can proceed.
                    second.store(true, Ordering::SeqCst);
                }
                let p = Props::new(move || StallingChild {
                    stop_gate: gate.clone(),
                    stop_started: started.clone(),
                });
                let p = p.with_idle_timeout(IdleTimeout::Persistent);
                // Explicitly no ctx.watch — this is the condition under test.
                ctx.spawn_child(p).await;
            }
        }

        fn on_terminated(
            &mut self,
            _: Terminated,
            _ctx: &mut ProcessContext<Self>,
        ) -> impl Future<Output = ()> + Send {
            let flag = self.on_terminated_called.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Receive<TriggerRestart> for RestartableNoWatch {
        type Response = ();
        type Error = HandlerError;
        async fn recv(
            &mut self,
            _: TriggerRestart,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), HandlerError> {
            Err(HandlerError)
        }
    }

    impl Receive<Barrier> for RestartableNoWatch {
        type Response = ();
        type Error = std::convert::Infallible;
        async fn recv(
            &mut self,
            _: Barrier,
            _ctx: &mut ProcessContext<Self>,
        ) -> Result<(), std::convert::Infallible> {
            Ok(())
        }
    }

    let sg = stop_gate.clone();
    let css = child_stop_started.clone();
    let fsd = first_start_done.clone();
    let ssd = second_start_done.clone();
    let otc = on_terminated_called.clone();

    let props = Props::new(move || RestartableNoWatch {
        stop_gate: sg.clone(),
        child_stop_started: css.clone(),
        first_start_done: fsd.clone(),
        second_start_done: ssd.clone(),
        on_terminated_called: otc.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));

    let proxy = system.spawn(props).await;

    // Wait for the first on_start to complete (child spawned).
    wait_for_flag(&first_start_done).await;

    // Trigger restart.
    proxy
        .tell(TriggerRestart)
        .await
        .expect("trigger restart must succeed");

    // Wait for the stalling child's on_stop to start (parent blocked in drain).
    wait_for_flag(&child_stop_started).await;

    // Unblock the first stalling child — it sends Terminated to the parent
    // (the parent is an implicit watcher via spawn_child). Since the parent
    // never called ctx.watch, explicit_watches does not contain the child, so
    // the Terminated is processed as bookkeeping-only (no on_terminated).
    stop_gate.notify_one();

    // Wait for the second on_start to complete (restart done).
    wait_for_flag(&second_start_done).await;

    // Barrier: ensure the parent loop has drained all pending signals.
    proxy.ask(Barrier).await.expect("barrier ask");

    // Assert: on_terminated must NOT have been called for the non-watched child.
    assert!(
        !on_terminated_called.load(Ordering::SeqCst),
        "ARCH-REVIEW-005 regression: Terminated (non-watched child stop \
         during restart) must NOT trigger on_terminated on the new process state"
    );

    // Cleanup: stop the parent and unblock the second stalling child.
    stop_gate.notify_one();
    proxy.stop().await.expect("stop parent after assertion");
}

// ---------------------------------------------------------------------------
// ARCH-REVIEW-019 regression: child terminating before Watch is registered
// must not leave a dead pid in ctx.children forever.
// ---------------------------------------------------------------------------

/// Regression for family_tag `call-chain-violation` (ARCH-REVIEW-019).
///
/// A child that terminates before the Watch signal it receives (or before the
/// registry lookup in `wiring::watch` finds it) must still be removed from the
/// parent's `ctx.children`.  With `register_child` now delegating to
/// `wiring::watch`, a missing child causes `deliver_not_found` to send
/// `Terminated { why: NotFound }` to the parent's `sys_tx`, so
/// `stop_children_and_wait` drains correctly and the parent exits without
/// deadlocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_terminating_before_watch_registered_does_not_deadlock_parent_stop() {
    let system = ProcessSystem::new().await;
    let parent_stopped = Arc::new(AtomicBool::new(false));
    let child_stopped = Arc::new(AtomicBool::new(false));

    struct QuickDyingChild {
        stopped: Arc<AtomicBool>,
    }

    impl Process for QuickDyingChild {
        fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let stopped = self.stopped.clone();
            async move {
                // Self-stop immediately so the parent's register_child may race
                // with this termination.
                stopped.store(true, Ordering::SeqCst);
                let _ = ctx.stop_self().await;
            }
        }
    }

    struct QuickChildParent {
        child_stopped: Arc<AtomicBool>,
        parent_stopped: Arc<AtomicBool>,
    }

    impl Process for QuickChildParent {
        fn on_start(
            &mut self,
            ctx: &mut ProcessContext<Self>,
        ) -> impl Future<Output = ()> + Send {
            let stopped = self.child_stopped.clone();
            async move {
                let props = Props::new(move || QuickDyingChild { stopped: stopped.clone() });
                let props = props.with_idle_timeout(IdleTimeout::Persistent);
                // The child may terminate before or during register_child's
                // wiring::watch call. Either way, the parent must not deadlock.
                ctx.spawn_child(props).await;
            }
        }

        fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
            let f = self.parent_stopped.clone();
            async move {
                f.store(true, Ordering::SeqCst);
            }
        }
    }

    let ps = parent_stopped.clone();
    let cs = child_stopped.clone();
    let props = Props::new(move || QuickChildParent {
        child_stopped: cs.clone(),
        parent_stopped: ps.clone(),
    });
    let props = props.with_idle_timeout(IdleTimeout::Persistent);
    let parent_proxy = system.spawn(props).await;

    // Wait for the child to have self-stopped.
    wait_for_flag(&child_stopped).await;

    // Stop the parent — must complete without deadlocking even if the child
    // had already terminated before the Watch was registered.
    parent_proxy.stop().await.expect("parent stop");
    wait_for_flag(&parent_stopped).await;
}
