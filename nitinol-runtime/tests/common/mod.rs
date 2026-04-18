use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::process::{Process, Receive};
use nitinol_runtime::{BoxError, Props};

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
    fn on_start(&mut self) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }

    fn on_stop(&mut self) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

/// Fire-and-forget message: increments the counter.
pub struct Increment;

impl Receive<Increment> for TrackedProcess {
    type Response = ();
    fn receive(&mut self, _msg: Increment) -> impl Future<Output = Result<(), BoxError>> + Send {
        self.counter.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    }
}

/// Request-response message: returns the current counter value.
pub struct GetCount;

impl Receive<GetCount> for TrackedProcess {
    type Response = u32;
    fn receive(&mut self, _msg: GetCount) -> impl Future<Output = Result<u32, BoxError>> + Send {
        let count = self.counter.load(Ordering::SeqCst);
        async move { Ok(count) }
    }
}

/// Message whose handler always fails.
pub struct FailingMessage;

impl Receive<FailingMessage> for TrackedProcess {
    type Response = ();
    fn receive(
        &mut self,
        _msg: FailingMessage,
    ) -> impl Future<Output = Result<(), BoxError>> + Send {
        async { Err("intentional failure".into()) }
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
