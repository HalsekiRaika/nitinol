//! Tests for Issue #56: unified `ProcessSystem::spawn` entry point.
//!
//! The pre-spec API exposed five spawn entry points
//! (`spawn` / `spawn_named` / `spawn_with_driver` / `spawn_named_with_driver` /
//! `spawn_stream`). The spec collapses them into ONE — `ps.spawn(_)` —
//! parameterized by what is being spawned:
//!
//! | Old                                       | New                                        |
//! |-------------------------------------------|--------------------------------------------|
//! | `ps.spawn(props)`                         | `ps.spawn(props)`                          |
//! | `ps.spawn_named(name, props)`             | `ps.spawn(props.with_name(name))`          |
//! | `ps.spawn_with_driver(props, driver)`     | `ps.spawn(props.with_driver(driver))`       |
//! | `ps.spawn_named_with_driver(...)`         | `ps.spawn(props.with_name(...).with_driver(...))` |
//! | `ps.spawn(nitinol_runtime::StreamProps::<T>::new(topic))`             | `ps.spawn(StreamProps::<T>::new(topic))`   |
//!
//! These tests pin down:
//! - `ps.spawn(props)` accepts a `Props<P>` and returns `ProcessProxy<P>`.
//! - `ps.spawn(stream_props)` accepts a `StreamProps<T>` and returns
//!   `ProcessProxy<Stream<T>>` — same `spawn` method, different argument type
//!   (the spec's `Spawnable` unification).
//! - A child spawned via `ctx.spawn_child(props.with_driver(...))` works —
//!   `spawn_child_with_driver` is collapsed too.
//! - Spawning a `StreamProps` whose topic is already registered surfaces a
//!   `SpawnError` (the same uniqueness contract `spawn_stream` provided
//!   pre-spec, preserved under the unified entry).
//!
//! StreamProps-specific behavior (Supervision::Resume immutability, capacity
//! delegation) lives in `stream_props.rs`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use nitinol_runtime::error::HandlerError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Driver, Process, ProcessContext, Receive};
use nitinol_runtime::{
    BoxedMessage, ProcessSystem, Props, Stream, StreamProps,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TickProcess {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
}

impl Process for TickProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
}

fn tick_props(ticks: Arc<AtomicU32>, started: Arc<AtomicBool>) -> Props<TickProcess> {
    Props::new(move || TickProcess {
        ticks: ticks.clone(),
        started: started.clone(),
    })
}

struct ChannelDriver {
    rx: mpsc::Receiver<()>,
}

impl Driver<TickProcess> for ChannelDriver {
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
}

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

// ---------------------------------------------------------------------------
// `ps.spawn(props)` — Props<P> path
// ---------------------------------------------------------------------------

/// Given a plain `Props<TickProcess>`,
/// when `system.spawn(props).await` runs,
/// then a `ProcessProxy<TickProcess>` is returned and `on_start` has fired.
#[tokio::test]
async fn spawn_with_props_starts_user_process() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));

    let _proxy = system
        .spawn(tick_props(ticks, Arc::clone(&started)))
        .await;

    wait_for_flag(&started).await;
}

// ---------------------------------------------------------------------------
// `ps.spawn(props.with_name(name))` — name absorbed into Props
// ---------------------------------------------------------------------------

/// Given `Props::with_name("unified-named")`,
/// when spawned via the unified entry,
/// then the process is discoverable via `lookup_by_name`.
/// Replaces the pre-spec `spawn_named`.
#[tokio::test]
async fn spawn_resolves_with_name_alias_in_registry() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let name = ProcessName::new("unified-named");

    let proxy = system
        .spawn(
            tick_props(ticks, Arc::clone(&started)).with_name(name.clone()),
        )
        .await;
    wait_for_flag(&started).await;

    let found = system
        .lookup_by_name(&name)
        .await
        .expect("with_name must register the alias in the registry");
    let typed = found
        .downcast::<TickProcess>()
        .expect("alias must resolve to the same process type");
    assert_eq!(typed.pid(), proxy.pid());
}

// ---------------------------------------------------------------------------
// `ps.spawn(props.with_driver(driver))` — replaces spawn_with_driver
// ---------------------------------------------------------------------------

/// Given `Props::with_driver(ChannelDriver)`,
/// when spawned via the unified entry and three events are pushed onto the
/// driver's channel,
/// then the driver's `apply` runs three times — proving the unified entry
/// composes the custom driver into the lifecycle loop just like the pre-spec
/// `spawn_with_driver` did.
#[tokio::test]
async fn spawn_with_added_driver_runs_custom_apply() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<()>(4);

    let _proxy = system
        .spawn(
            tick_props(Arc::clone(&ticks), Arc::clone(&started)).with_driver(ChannelDriver { rx }),
        )
        .await;
    wait_for_flag(&started).await;

    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");
    tx.send(()).await.expect("driver source send");

    wait_for_count(&ticks, 3).await;
}

// ---------------------------------------------------------------------------
// `ps.spawn(props.with_name(...).with_driver(...))` — fully unified
// ---------------------------------------------------------------------------

/// Given `Props::with_name(...).with_driver(...)` (the spec's collapsing of
/// `spawn_named_with_driver`),
/// when spawned via the unified entry,
/// then the process is both discoverable by name AND driven by the custom
/// driver — neither feature is lost in the unification.
#[tokio::test]
async fn spawn_with_name_and_added_driver_runs_both_features() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<()>(4);
    let name = ProcessName::new("named-with-driver-unified");

    let proxy = system
        .spawn(
            tick_props(Arc::clone(&ticks), Arc::clone(&started))
                .with_name(name.clone())
                .with_driver(ChannelDriver { rx }),
        )
        .await;
    wait_for_flag(&started).await;

    tx.send(()).await.expect("driver source send");
    wait_for_count(&ticks, 1).await;

    let found = system
        .lookup_by_name(&name)
        .await
        .expect("with_name must register the alias in the registry");
    let typed = found
        .downcast::<TickProcess>()
        .expect("alias must resolve to the same process type");
    assert_eq!(typed.pid(), proxy.pid());
}

// ---------------------------------------------------------------------------
// `ps.spawn(StreamProps::<T>::new(topic))` — replaces spawn_stream
// ---------------------------------------------------------------------------

/// Given a `StreamProps::<BoxedMessage>::new("stream-via-unified")`,
/// when spawned via the unified entry,
/// then a `ProcessProxy<Stream<BoxedMessage>>` is returned and the topic is
/// registered in the registry. Replaces the pre-spec `spawn_stream`.
#[tokio::test]
async fn spawn_with_stream_props_returns_stream_proxy_and_registers_topic() {
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("stream-via-unified");

    let proxy = system
        .spawn(StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("spawn(StreamProps) must succeed for a fresh topic");

    // The proxy must accept publishes — proving it really is a Stream proxy.
    proxy
        .publish_boxed(42u32)
        .await
        .expect("publish must succeed on the spawned stream");

    let found = system
        .lookup_by_name(&topic)
        .await
        .expect("topic must be registered after spawn(StreamProps)");
    let typed = found
        .downcast::<Stream<BoxedMessage>>()
        .expect("registered topic must resolve to a Stream<BoxedMessage> proxy");
    assert_eq!(typed.pid(), proxy.pid());
}

/// Given a topic that was already spawned once,
/// when `ps.spawn(StreamProps::<T>::new(topic))` runs a second time with the
/// same name,
/// then the second call surfaces a `SpawnError` — preserving the unique-
/// per-topic contract that `spawn_stream` used to enforce.
#[tokio::test]
async fn spawn_with_stream_props_rejects_duplicate_topic() {
    let system = ProcessSystem::new().await;
    let topic = ProcessName::new("stream-dup-unified");

    let _first = system
        .spawn(StreamProps::<BoxedMessage>::new(topic.clone()))
        .await
        .expect("first spawn must succeed");

    let second = system
        .spawn(StreamProps::<BoxedMessage>::new(topic.clone()))
        .await;

    assert!(
        second.is_err(),
        "spawn(StreamProps) with a duplicate topic must return Err"
    );
}

// ---------------------------------------------------------------------------
// `ctx.spawn_child(props.with_driver(...))` — child path mirrors the system
// path (no `spawn_child_with_driver`).
// ---------------------------------------------------------------------------

/// Child process whose Driver bumps a counter through `with_driver`.
struct ChildTick {
    ticks: Arc<AtomicU32>,
}

impl Process for ChildTick {}

struct ChildTickDriver {
    rx: mpsc::Receiver<()>,
}

impl Driver<ChildTick> for ChildTickDriver {
    type Event = ();
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        self.rx.recv()
    }
    async fn apply(
        &mut self,
        state: &mut ChildTick,
        _ctx: &mut ProcessContext<ChildTick>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        state.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Parent process that spawns one child via `ctx.spawn_child(props.with_driver(driver))`.
struct Parent {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    /// Sender end of the child's driver, captured here so the test can push
    /// events after spawn.
    child_tx: tokio::sync::Mutex<Option<mpsc::Sender<()>>>,
}

struct SpawnChild;
struct GetChildSender;

impl Process for Parent {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
}

impl Receive<SpawnChild> for Parent {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: SpawnChild,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let (tx, rx) = mpsc::channel::<()>(4);
        let ticks = self.ticks.clone();
        let child_props = Props::new(move || ChildTick {
            ticks: ticks.clone(),
        })
        .with_driver(ChildTickDriver { rx });
        let _child_proxy = ctx.spawn_child(child_props).await;
        *self.child_tx.lock().await = Some(tx);
        Ok(())
    }
}

impl Receive<GetChildSender> for Parent {
    type Response = mpsc::Sender<()>;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: GetChildSender,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<mpsc::Sender<()>, std::convert::Infallible> {
        Ok(self
            .child_tx
            .lock()
            .await
            .clone()
            .expect("SpawnChild must run before GetChildSender"))
    }
}

/// Given a parent process that calls `ctx.spawn_child(props.with_driver(driver))`,
/// when a tick is sent on the captured driver channel,
/// then the child's `apply` runs — proving the child path accepts
/// `with_driver` the same way the system path does (so `spawn_child_with_driver`
/// is genuinely redundant after the unification).
#[tokio::test]
async fn ctx_spawn_child_with_added_driver_runs_apply() {
    let system = ProcessSystem::new().await;
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));

    let parent_proxy = system
        .spawn(Props::new({
            let ticks = ticks.clone();
            let started = started.clone();
            move || Parent {
                ticks: ticks.clone(),
                started: started.clone(),
                child_tx: tokio::sync::Mutex::new(None),
            }
        }))
        .await;
    wait_for_flag(&started).await;

    parent_proxy
        .tell(SpawnChild)
        .await
        .expect("tell SpawnChild must succeed");
    let tx = parent_proxy
        .ask(GetChildSender)
        .await
        .expect("ask GetChildSender must succeed");

    tx.send(()).await.expect("child driver send must succeed");

    wait_for_count(&ticks, 1).await;
}
