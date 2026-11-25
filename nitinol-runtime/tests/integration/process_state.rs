//! Integration tests for a process's lifecycle state and the mailbox closing
//! when the process starts stopping.
//!
//! These tests pin down:
//! - `ProcessProxy::state()` reads `Starting` while `on_start` runs, `Running`
//!   after it, `Stopping` from the moment the loop is left (so while `on_stop`
//!   runs), and `Stopped` once the process has left the registry — on one
//!   proxy and every clone of it
//! - `ProcessState` is a `Copy` value named both from the crate root and from
//!   `process`
//! - a `tell` / `ask` to a stopping process goes to the dead letters, and the
//!   `ask` returns `AskError::DeadLetter` instead of waiting
//! - a parent stopped while its child's `ask` sits in the parent's mailbox
//!   drops that ask (`ReplyDropped`), so the child can take its `Stop` and the
//!   parent's wait for its children ends
//! - a supervision restart is not a stop: the state stays `Running` and the
//!   mailbox stays open

use std::convert::Infallible;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use nitinol_runtime::error::{AskError, HandlerError, SendError};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, Receive, SubscriberContext, Terminated,
    TerminatedReason,
};
use nitinol_runtime::{
    BoxedMessage, DeadLetter, IdleTimeout, ProcessState, ProcessSystem, Props, Subscriber,
    SupervisionStrategy,
};

/// How long any one wait here may take before the test fails. A hang — the
/// mutual wait this file guards against — shows up as this running out.
const LIMIT: Duration = Duration::from_secs(5);

const FIXTURE_LOCK: &str = "a fixture lock guards only a read or a push, so no holder panics";

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_until_unregistered(system: &ProcessSystem, pid: Pid) {
    let deadline = Instant::now() + LIMIT;
    while system.lookup(pid).await.is_some() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {pid} to leave the registry"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A point a hook or handler stops at until the test lets it through.
#[derive(Clone, Default)]
struct Gate {
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
}

impl Gate {
    async fn pass(&self) {
        self.entered.store(true, Ordering::SeqCst);
        self.release.notified().await;
    }

    async fn wait_entered(&self, what: &str) {
        let entered = self.entered.clone();
        wait_until(what, move || entered.load(Ordering::SeqCst)).await;
    }

    /// Lets the one holder through, or the next one to arrive if none is
    /// waiting yet.
    fn open(&self) {
        self.release.notify_one();
    }
}

struct Ping;

// A watcher that keeps every `Terminated` it is told of

type TerminationLog = Arc<Mutex<Vec<Terminated>>>;

struct Watcher {
    log: TerminationLog,
}

struct WatchPid(Pid);

impl Process for Watcher {
    async fn on_terminated(&mut self, terminated: Terminated, _ctx: &mut ProcessContext<Self>) {
        self.log.lock().expect(FIXTURE_LOCK).push(terminated);
    }
}

impl Receive<WatchPid> for Watcher {
    type Response = ();
    type Error = Infallible;
    async fn recv(
        &mut self,
        msg: WatchPid,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        ctx.watch(msg.0).await;
        Ok(())
    }
}

/// Spawns a watcher of every pid in `pids`, and answers what it is told.
///
/// Each watch has reached the watched process's signal queue once its `ask`
/// returns, so a stop requested afterwards is seen after the watch.
async fn watch_all(system: &ProcessSystem, pids: &[Pid]) -> TerminationLog {
    let log: TerminationLog = Arc::new(Mutex::new(Vec::new()));
    let watcher = system
        .spawn(
            Props::new({
                let log = log.clone();
                move || Watcher { log: log.clone() }
            })
            .with_idle_timeout(IdleTimeout::Persistent),
        )
        .await;
    for pid in pids {
        watcher
            .ask(WatchPid(*pid))
            .await
            .expect("the watcher accepts a watch request");
    }
    log
}

fn terminated_reason(log: &TerminationLog, pid: Pid) -> Option<TerminatedReason> {
    log.lock()
        .expect(FIXTURE_LOCK)
        .iter()
        .find(|t| t.who == pid)
        .map(|t| t.why)
}

// A process whose `on_start` and `on_stop` wait at a gate

struct GatedLifecycle {
    start: Gate,
    stop: Gate,
    /// What the process's own proxy read as `on_stop` began.
    state_in_on_stop: Arc<Mutex<Option<ProcessState>>>,
}

impl Process for GatedLifecycle {
    async fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) {
        self.start.pass().await;
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext<Self>) {
        *self.state_in_on_stop.lock().expect(FIXTURE_LOCK) = Some(ctx.self_proxy().state());
        self.stop.pass().await;
    }
}

impl Receive<Ping> for GatedLifecycle {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Ping, _ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        Ok(())
    }
}

fn gated_props(
    start: &Gate,
    stop: &Gate,
    state_in_on_stop: &Arc<Mutex<Option<ProcessState>>>,
) -> Props<GatedLifecycle> {
    let start = start.clone();
    let stop = stop.clone();
    let state_in_on_stop = state_in_on_stop.clone();
    Props::new(move || GatedLifecycle {
        start: start.clone(),
        stop: stop.clone(),
        state_in_on_stop: state_in_on_stop.clone(),
    })
    .with_idle_timeout(IdleTimeout::Persistent)
}

#[tokio::test]
async fn process_state_is_a_copy_value_named_from_the_root_and_the_process_module() {
    // Given: a started process
    let system = ProcessSystem::new().await;
    let (start, stop) = (Gate::default(), Gate::default());
    start.open();
    let proxy = system
        .spawn(gated_props(&start, &stop, &Arc::new(Mutex::new(None))))
        .await;

    // When: its state is read and copied between the two names of the type
    let read: ProcessState = proxy.state();
    let copied: nitinol_runtime::process::ProcessState = read;

    // Then: both copies stay usable and agree
    assert_eq!(read, copied);
    assert!(
        matches!(read, ProcessState::Starting | ProcessState::Running),
        "a process that was just spawned is starting or running, got {read:?}"
    );
}

#[tokio::test]
async fn state_moves_from_starting_through_running_and_stopping_to_stopped() {
    // Given: a process held inside `on_start`
    let system = ProcessSystem::new().await;
    let (start, stop) = (Gate::default(), Gate::default());
    let state_in_on_stop = Arc::new(Mutex::new(None));
    let proxy = system
        .spawn(gated_props(&start, &stop, &state_in_on_stop))
        .await;
    let pid = proxy.pid();
    let observer = proxy.clone();
    start.wait_entered("on_start to begin").await;

    // Then: it is starting, on the proxy and on every clone of it
    assert_eq!(proxy.state(), ProcessState::Starting);
    assert_eq!(observer.state(), ProcessState::Starting);

    // When: on_start finishes
    start.open();

    // Then: it is running
    wait_until("the process to be running", || {
        observer.state() == ProcessState::Running
    })
    .await;
    assert_eq!(proxy.state(), ProcessState::Running);

    // When: it is stopped and held inside `on_stop`
    let log = watch_all(&system, &[pid]).await;
    proxy
        .stop()
        .await
        .expect("a running process accepts a stop");
    stop.wait_entered("on_stop to begin").await;

    // Then: it is stopping — already as on_stop began — and still registered
    assert_eq!(observer.state(), ProcessState::Stopping);
    assert_eq!(proxy.state(), ProcessState::Stopping);
    assert_eq!(
        *state_in_on_stop.lock().expect(FIXTURE_LOCK),
        Some(ProcessState::Stopping),
        "on_stop runs after the process has entered Stopping"
    );
    assert!(
        system.lookup(pid).await.is_some(),
        "a stopping process is still registered"
    );

    // When: on_stop finishes
    stop.open();

    // Then: once its watcher is told it terminated, it has left the registry
    // and is stopped
    wait_until("the watcher to be told the process terminated", || {
        terminated_reason(&log, pid).is_some()
    })
    .await;
    assert_eq!(
        terminated_reason(&log, pid),
        Some(TerminatedReason::Stopped)
    );
    assert!(system.lookup(pid).await.is_none());
    assert_eq!(observer.state(), ProcessState::Stopped);
    assert_eq!(proxy.state(), ProcessState::Stopped);
}

// Dead letters of a stopping process

/// Keeps the destination of every `DeadLetter` published.
struct DeadLetterDestinations {
    seen: Arc<Mutex<Vec<Pid>>>,
}

impl Subscriber<BoxedMessage> for DeadLetterDestinations {
    fn recv(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut SubscriberContext<'_, BoxedMessage>,
    ) -> impl Future<Output = ()> + Send {
        if let Some(dl) = msg.downcast_ref::<DeadLetter>() {
            self.seen.lock().expect(FIXTURE_LOCK).push(dl.destination);
        }
        async {}
    }
}

#[tokio::test]
async fn tell_and_ask_to_a_stopping_process_go_to_the_dead_letters() {
    // Given: a dead-letter subscriber
    let system = ProcessSystem::new().await;
    let destinations = Arc::new(Mutex::new(Vec::new()));
    let subscriber = system
        .spawn(Props::subscriber({
            let seen = destinations.clone();
            move || DeadLetterDestinations { seen: seen.clone() }
        }))
        .await;
    system
        .dead_letter_stream()
        .subscribe(subscriber)
        .await
        .expect("subscribing to the dead letters succeeds");

    // And: a process that has been stopped and is held inside `on_stop`
    let (start, stop) = (Gate::default(), Gate::default());
    start.open();
    let proxy = system
        .spawn(gated_props(&start, &stop, &Arc::new(Mutex::new(None))))
        .await;
    let pid = proxy.pid();
    start.wait_entered("on_start to begin").await;
    proxy
        .stop()
        .await
        .expect("a running process accepts a stop");
    stop.wait_entered("on_stop to begin").await;

    // When: a tell is sent to it
    let told = proxy.tell(Ping).await;

    // Then: it is refused
    assert!(
        matches!(told, Err(SendError)),
        "a tell to a stopping process must fail, got {told:?}"
    );

    // When: an ask is sent to it
    let asked = tokio::time::timeout(LIMIT, proxy.ask(Ping))
        .await
        .expect("an ask to a stopping process must not wait for a reply");

    // Then: it fails as a dead letter addressed to the process
    match asked {
        Err(AskError::DeadLetter { destination }) => assert_eq!(destination, pid),
        other => panic!("expected AskError::DeadLetter {{ destination: {pid} }}, got {other:?}"),
    }

    // And: both messages reached the dead letters, addressed to the process
    wait_until("both messages to reach the dead letters", || {
        destinations.lock().expect(FIXTURE_LOCK).len() >= 2
    })
    .await;
    assert_eq!(*destinations.lock().expect(FIXTURE_LOCK), vec![pid, pid]);

    // And: the process still finishes stopping
    stop.open();
    wait_until_unregistered(&system, pid).await;
}

// A child asking its parent while the parent stops

/// Holds the parent inside a handler, so a message sent meanwhile waits in
/// its mailbox.
struct Hold;

struct AskedParent {
    hold: Gate,
    go: Arc<Notify>,
    asking: Arc<AtomicBool>,
    outcome: AskOutcome,
    child: Arc<Mutex<Option<Pid>>>,
}

type AskOutcome = Arc<Mutex<Option<Result<(), AskError<Infallible>>>>>;

impl Process for AskedParent {
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        let asker = ParentAsker {
            parent: ctx.self_proxy().clone(),
            go: self.go.clone(),
            asked: false,
            asking: self.asking.clone(),
            outcome: self.outcome.clone(),
        };
        let child = ctx
            .spawn_child(
                Props::new(|| AskingChild)
                    .with_idle_timeout(IdleTimeout::Persistent)
                    .with_driver(asker),
            )
            .await;
        *self.child.lock().expect(FIXTURE_LOCK) = Some(child.pid());
    }
}

impl Receive<Hold> for AskedParent {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Hold, _ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        self.hold.pass().await;
        Ok(())
    }
}

impl Receive<Ping> for AskedParent {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Ping, _ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        Ok(())
    }
}

struct AskingChild;

impl Process for AskingChild {}

/// Asks the parent once, from inside `apply`, when the test says go.
struct ParentAsker {
    parent: ProcessProxy<AskedParent>,
    go: Arc<Notify>,
    asked: bool,
    asking: Arc<AtomicBool>,
    outcome: AskOutcome,
}

impl Driver<AskingChild> for ParentAsker {
    type Event = ();

    async fn next(&mut self) -> Option<()> {
        if self.asked {
            std::future::pending::<()>().await;
        }
        self.go.notified().await;
        self.asked = true;
        Some(())
    }

    async fn apply(
        &mut self,
        _state: &mut AskingChild,
        _ctx: &mut ProcessContext<AskingChild>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        let mut ask = pin!(self.parent.ask(Ping));
        // The first poll puts the ask in the parent's mailbox and parks on the
        // reply, so once `asking` is seen the ask is queued, not about to be.
        let first = std::future::poll_fn(|cx| Poll::Ready(ask.as_mut().poll(cx))).await;
        self.asking.store(true, Ordering::SeqCst);
        let outcome = match first {
            Poll::Ready(outcome) => outcome,
            Poll::Pending => ask.await,
        };
        *self.outcome.lock().expect(FIXTURE_LOCK) = Some(outcome);
        Ok(())
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn stopping_a_parent_holding_its_childs_ask_terminates_both() {
    // Given: a parent whose child asks it from inside the child's `apply`
    let system = ProcessSystem::new().await;
    let hold = Gate::default();
    let go = Arc::new(Notify::new());
    let asking = Arc::new(AtomicBool::new(false));
    let outcome: AskOutcome = Arc::new(Mutex::new(None));
    let child_slot = Arc::new(Mutex::new(None));
    let parent = system
        .spawn(
            Props::new({
                let hold = hold.clone();
                let go = go.clone();
                let asking = asking.clone();
                let outcome = outcome.clone();
                let child = child_slot.clone();
                move || AskedParent {
                    hold: hold.clone(),
                    go: go.clone(),
                    asking: asking.clone(),
                    outcome: outcome.clone(),
                    child: child.clone(),
                }
            })
            .with_idle_timeout(IdleTimeout::Persistent),
        )
        .await;
    wait_until("the parent to spawn its child", || {
        child_slot.lock().expect(FIXTURE_LOCK).is_some()
    })
    .await;
    let child = child_slot
        .lock()
        .expect(FIXTURE_LOCK)
        .expect("the child pid was just seen");
    let log = watch_all(&system, &[parent.pid(), child]).await;

    // And: the parent is busy, and the child's ask waits in its mailbox
    parent.tell(Hold).await.expect("the parent accepts Hold");
    hold.wait_entered("the parent to be holding").await;
    go.notify_one();
    let flag = asking.clone();
    wait_until("the child to be asking its parent", move || {
        flag.load(Ordering::SeqCst)
    })
    .await;

    // When: the parent is stopped and then lets go of what it was handling
    parent
        .stop()
        .await
        .expect("a running parent accepts a stop");
    hold.open();

    // Then: both are told terminated within the limit, the parent having
    // waited for its child
    wait_until("both the parent and the child to terminate", || {
        terminated_reason(&log, parent.pid()).is_some() && terminated_reason(&log, child).is_some()
    })
    .await;
    assert_eq!(
        terminated_reason(&log, parent.pid()),
        Some(TerminatedReason::Stopped)
    );
    assert_eq!(
        terminated_reason(&log, child),
        Some(TerminatedReason::Stopped)
    );

    // And: the child's ask was dropped with the parent's mailbox
    let outcome = outcome.lock().expect(FIXTURE_LOCK).take();
    assert!(
        matches!(outcome, Some(Err(AskError::ReplyDropped))),
        "the child's queued ask must return ReplyDropped, got {outcome:?}"
    );
}

// A supervision restart is not a stop

struct FailOnce;

struct Restarting {
    starts: Arc<AtomicU32>,
    states_in_on_stop: Arc<Mutex<Vec<ProcessState>>>,
}

impl Process for Restarting {
    async fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) {
        self.starts.fetch_add(1, Ordering::SeqCst);
    }

    async fn on_stop(&mut self, ctx: &mut ProcessContext<Self>) {
        self.states_in_on_stop
            .lock()
            .expect(FIXTURE_LOCK)
            .push(ctx.self_proxy().state());
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the fixture handler fails on purpose")]
struct Failed;

impl Receive<FailOnce> for Restarting {
    type Response = ();
    type Error = Failed;
    async fn recv(&mut self, _: FailOnce, _ctx: &mut ProcessContext<Self>) -> Result<(), Failed> {
        Err(Failed)
    }
}

impl Receive<Ping> for Restarting {
    type Response = ();
    type Error = Infallible;
    async fn recv(&mut self, _: Ping, _ctx: &mut ProcessContext<Self>) -> Result<(), Infallible> {
        Ok(())
    }
}

#[tokio::test]
async fn a_supervision_restart_stays_running_with_its_mailbox_open() {
    // Given: a process supervised by Restart
    let system = ProcessSystem::new().await;
    let starts = Arc::new(AtomicU32::new(0));
    let states_in_on_stop = Arc::new(Mutex::new(Vec::new()));
    let proxy = system
        .spawn(
            Props::new({
                let starts = starts.clone();
                let states = states_in_on_stop.clone();
                move || Restarting {
                    starts: starts.clone(),
                    states_in_on_stop: states.clone(),
                }
            })
            .with_idle_timeout(IdleTimeout::Persistent)
            .with_supervision_strategy(
                SupervisionStrategy::restart(5, Duration::from_secs(10))
                    .expect("valid restart config"),
            ),
        )
        .await;

    // When: a handler fails and the process restarts
    proxy
        .tell(FailOnce)
        .await
        .expect("a running process accepts a tell");
    wait_until("the process to restart", || {
        starts.load(Ordering::SeqCst) == 2
    })
    .await;

    // Then: the restart's on_stop ran while the process was running
    assert_eq!(
        *states_in_on_stop.lock().expect(FIXTURE_LOCK),
        vec![ProcessState::Running]
    );

    // And: the restarted process is running and still answers
    assert_eq!(proxy.state(), ProcessState::Running);
    tokio::time::timeout(LIMIT, proxy.ask(Ping))
        .await
        .expect("a restarted process answers within the limit")
        .expect("a restarted process keeps its mailbox open");
}
