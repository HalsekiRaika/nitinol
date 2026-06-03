use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{IdleTimeout, Props, Terminated};

/// Test error type for handlers that intentionally fail.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TestError(pub String);

/// Test process that tracks lifecycle events and message counts
/// through shared atomic state.
pub struct TrackedProcess {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    counter: Arc<AtomicU32>,
}

impl TrackedProcess {
    pub fn new(
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        counter: Arc<AtomicU32>,
    ) -> Self {
        Self {
            started,
            stopped,
            counter,
        }
    }
}

impl Process for TrackedProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

/// Fire-and-forget message: increments the counter.
pub struct Increment;

impl Receive<Increment> for TrackedProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: Increment,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Request-response message: returns the current counter value.
pub struct GetCount;

impl Receive<GetCount> for TrackedProcess {
    type Response = u32;
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _msg: GetCount,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<u32, std::convert::Infallible> {
        let count = self.counter.load(Ordering::SeqCst);
        Ok(count)
    }
}

/// Message whose handler always fails.
pub struct FailingMessage;

impl Receive<FailingMessage> for TrackedProcess {
    type Response = ();
    type Error = TestError;
    async fn recv(
        &mut self,
        _msg: FailingMessage,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), TestError> {
        Err(TestError("intentional failure".to_string()))
    }
}

/// Creates a fresh set of shared atomic state for a TrackedProcess.
pub fn tracked_state() -> (Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicU32>) {
    (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU32::new(0)),
    )
}

/// Creates Props<TrackedProcess> wired to the given shared state.
pub fn test_props(
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    counter: Arc<AtomicU32>,
) -> Props<TrackedProcess> {
    Props::new(move || TrackedProcess::new(started.clone(), stopped.clone(), counter.clone()))
}

/// Waits for an AtomicBool flag to become true, with a 5-second timeout.
/// Panics if the flag does not become true within the timeout.
// Not all test binaries use this helper; suppress dead_code for those
#[allow(dead_code)]
pub async fn wait_for_flag(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for flag to become true"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---- Watcher process helpers ----
// Not all test binaries use these; suppress dead_code for those

/// Watcher process that records the first `Terminated` notification it receives.
#[allow(dead_code)]
pub struct WatcherProcess {
    pub received: Arc<Mutex<Option<Terminated>>>,
}

/// Tell the watcher to start watching the given PID.
#[allow(dead_code)]
pub struct WatchPid {
    pub pid: Pid,
}

/// No-op barrier: when the ask returns, the watcher has processed all prior messages.
#[allow(dead_code)]
pub struct Barrier;

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

/// Creates `Props<WatcherProcess>` wired to the given shared state.
///
/// Pass `IdleTimeout::Persistent` when the watcher must outlive a system that
/// has a short default idle timeout. Pass `IdleTimeout::Inherit` for tests where
/// the watcher's lifetime is controlled by the system default or explicit stop.
#[allow(dead_code)]
pub fn watcher_props(
    received: Arc<Mutex<Option<Terminated>>>,
    idle_timeout: IdleTimeout,
) -> Props<WatcherProcess> {
    let mut props = Props::new(move || WatcherProcess {
        received: received.clone(),
    });
    props.with_idle_timeout(idle_timeout);
    props
}

/// Polls until `received` is `Some`, with a 5-second timeout.
/// Panics if no `Terminated` notification arrives within the timeout.
#[allow(dead_code)]
pub async fn wait_for_terminated(received: &Arc<Mutex<Option<Terminated>>>) {
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
