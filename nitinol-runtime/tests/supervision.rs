mod common;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props, SupervisionStrategy};

use common::TestError;

/// Process that counts on_start and on_stop calls, and fails on demand.
struct SupervisedProcess {
    start_count: Arc<AtomicU32>,
    stop_count: Arc<AtomicU32>,
}

impl Process for SupervisedProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.start_count.fetch_add(1, Ordering::SeqCst);
        async {}
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.stop_count.fetch_add(1, Ordering::SeqCst);
        async {}
    }
}

/// Message whose handler always returns Err (triggers supervision).
struct Fail;

impl Receive<Fail> for SupervisedProcess {
    type Response = ();
    type Error = TestError;
    async fn recv(&mut self, _: Fail, _ctx: &mut ProcessContext) -> Result<(), TestError> {
        Err(TestError("intentional failure".to_string()))
    }
}

/// Message whose handler always succeeds (used as synchronization barrier).
struct Ping;

impl Receive<Ping> for SupervisedProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        _: Ping,
        _ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

/// Waits for an AtomicU32 to reach at least `expected`, with a 5-second timeout.
async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for count to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Creates Props<SupervisedProcess> with the given strategy and shared state.
fn supervised_props(
    start_count: Arc<AtomicU32>,
    stop_count: Arc<AtomicU32>,
    strategy: SupervisionStrategy,
) -> Props<SupervisedProcess> {
    let mut props = Props::new(move || SupervisedProcess {
        start_count: start_count.clone(),
        stop_count: stop_count.clone(),
    });
    props.with_supervision_strategy(strategy);
    props
}

/// Stop strategy: a handler failure causes the process to stop cleanly.
/// on_stop is called exactly once; no restart occurs.
#[tokio::test]
async fn stop_strategy_failure_stops_process() {
    // Given: a process with the Stop supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Stop,
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;

    // Then: on_stop is called (process stops cleanly)
    wait_for_count(&stop_count, 1).await;

    // And: on_start was called exactly once (no restart)
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(start_count.load(Ordering::SeqCst), 1);
}

/// Stop strategy: the process is unregistered from the registry after failure.
#[tokio::test]
async fn stop_strategy_failure_unregisters_process() {
    // Given: a process with the Stop supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Stop,
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;
    wait_for_count(&stop_count, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the process is unregistered from the registry
    let found = system.lookup(pid).await;
    assert!(found.is_none(), "stopped process should be unregistered");
}

/// Restart strategy: a handler failure restarts the process (on_start called again).
#[tokio::test]
async fn restart_strategy_failure_restarts_process() {
    // Given: a process with the Restart supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 5,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;

    // Then: on_start is called again (process restarted)
    wait_for_count(&start_count, 2).await;
    assert_eq!(start_count.load(Ordering::SeqCst), 2);
}

/// Restart strategy: on_stop is called on the old instance before each restart.
#[tokio::test]
async fn restart_strategy_calls_on_stop_before_restart() {
    // Given: a process with the Restart strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 5,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: a failing message triggers a restart
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // Then: on_stop was called once (before the restart)
    assert_eq!(stop_count.load(Ordering::SeqCst), 1);
}

/// Restart strategy: the PID remains the same after a restart.
#[tokio::test]
async fn restart_preserves_pid() {
    // Given: a process with the Restart strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 5,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    let original_pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // When: the process fails and restarts
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // Then: the PID is unchanged
    assert_eq!(proxy.pid(), original_pid);
}

/// Restart strategy: the process remains registered in the registry after restart.
#[tokio::test]
async fn restart_process_remains_in_registry() {
    // Given: a process with the Restart strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 5,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // When: the process fails and restarts
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // Then: the process is still registered
    let found = system.lookup(pid).await;
    assert!(
        found.is_some(),
        "restarted process should remain in registry"
    );
}

/// Restart strategy: after restart, the new instance handles subsequent messages normally.
#[tokio::test]
async fn restarted_process_handles_subsequent_messages() {
    // Given: a process with the Restart strategy that has restarted once
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 5,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: the process fails and restarts
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // Then: the restarted instance handles new messages without error
    let result = proxy.ask(Ping).await;
    assert!(
        result.is_ok(),
        "restarted process should handle new messages"
    );
}

/// Rate limiting: exceeding max_retries within the window causes the process to stop permanently.
///
/// With max_retries=2:
///   - Fail #1 → restart (count=1, 1≤2 → allowed). start=2, stop=1.
///   - Fail #2 → restart (count=2, 2≤2 → allowed). start=3, stop=2.
///   - Fail #3 → stop    (count=3, 3>2 → denied).  start=3, stop=3.
#[tokio::test]
async fn restart_rate_limit_exceeded_causes_permanent_stop() {
    // Given: a process with Restart{max_retries: 2, within: 10s}
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 2,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // Trigger 2 restarts (within max_retries limit)
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 3).await;

    // Trigger 1 more failure (exceeds rate limit → permanent stop)
    let _ = proxy.tell(Fail).await;

    // Then: the process stops permanently (stop_count: 2 pre-restart + 1 final = 3)
    wait_for_count(&stop_count, 3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // And: no further restarts occurred
    assert_eq!(start_count.load(Ordering::SeqCst), 3);

    // And: the process is unregistered
    let found = system.lookup(pid).await;
    assert!(
        found.is_none(),
        "process should be unregistered after rate limit exceeded"
    );
}

/// Rate limiting: failures outside the window do not count against the limit.
/// A fresh restart is allowed once the window has expired.
#[tokio::test]
async fn restart_rate_limit_resets_after_window_expires() {
    // Given: a process with Restart{max_retries: 1, within: 50ms} (very short window)
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 1,
            within: Duration::from_millis(50),
        },
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: 1 failure occurs (uses up the 1 allowed restart within the window)
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // And: the 50ms window expires
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: another failure should be allowed (old timestamp pruned)
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 3).await;

    // And: the process is still alive after the second restart
    let result = proxy.ask(Ping).await;
    assert!(
        result.is_ok(),
        "process should still be alive after window-reset restart"
    );
}

/// Rate limiting: exactly at the limit — the last allowed restart succeeds,
/// the next failure causes a permanent stop.
#[tokio::test]
async fn restart_at_exact_rate_limit_boundary() {
    // Given: a process with Restart{max_retries: 1, within: 10s}
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Restart {
            max_retries: 1,
            within: Duration::from_secs(10),
        },
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // When: exactly 1 failure (within limit → restart)
    let _ = proxy.tell(Fail).await;
    wait_for_count(&start_count, 2).await;

    // Then: process is still alive after 1 restart
    let result = proxy.ask(Ping).await;
    assert!(
        result.is_ok(),
        "process should be alive after 1 restart (within limit)"
    );

    // When: a second failure (exceeds limit → permanent stop)
    let _ = proxy.tell(Fail).await;
    wait_for_count(&stop_count, 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: no further restart
    assert_eq!(start_count.load(Ordering::SeqCst), 2);
    let found = system.lookup(pid).await;
    assert!(
        found.is_none(),
        "process should be unregistered after exceeding rate limit"
    );
}

/// OneForOne: a failure in one process does not affect sibling processes.
#[tokio::test]
async fn failure_in_one_process_does_not_affect_sibling() {
    // Given: two independent processes
    let system = ProcessSystem::new().await;

    // Process A with Stop strategy (will fail)
    let start_a = Arc::new(AtomicU32::new(0));
    let stop_a = Arc::new(AtomicU32::new(0));
    let props_a = supervised_props(start_a.clone(), stop_a.clone(), SupervisionStrategy::Stop);
    let proxy_a = system.spawn(props_a).await;
    wait_for_count(&start_a, 1).await;

    // Process B with Stop strategy (will not fail)
    let start_b = Arc::new(AtomicU32::new(0));
    let stop_b = Arc::new(AtomicU32::new(0));
    let props_b = supervised_props(start_b.clone(), stop_b.clone(), SupervisionStrategy::Stop);
    let proxy_b = system.spawn(props_b).await;
    wait_for_count(&start_b, 1).await;

    // When: process A fails
    let _ = proxy_a.tell(Fail).await;
    wait_for_count(&stop_a, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: process B is unaffected and still handles messages
    let result = proxy_b.ask(Ping).await;
    assert!(
        result.is_ok(),
        "sibling process should be unaffected by another's failure"
    );
    assert_eq!(
        stop_b.load(Ordering::SeqCst),
        0,
        "sibling should not have been stopped"
    );
}

/// Props::into_parts panics when `within` is Duration::ZERO, preventing silent
/// rate-limit bypass where retain drops all timestamps and the limit is never enforced.
#[tokio::test]
#[should_panic(expected = "SupervisionStrategy::Restart requires `within` > 0")]
async fn restart_strategy_with_zero_within_panics() {
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count,
        stop_count,
        SupervisionStrategy::Restart {
            max_retries: 3,
            within: Duration::ZERO,
        },
    );
    system.spawn(props).await;
}

/// Resume strategy: a handler failure is ignored and the process continues running.
/// Subsequent messages are handled normally without any restart.
#[tokio::test]
async fn resume_strategy_ignores_handler_error() {
    // Given: a process with the Resume supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Resume,
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;

    // Then: the process is still alive and handles subsequent messages
    // (Ping after tell(Fail) is a barrier — if it succeeds, Fail was already processed)
    let result = proxy.ask(Ping).await;
    assert!(
        result.is_ok(),
        "process should still handle messages after a handler error under Resume"
    );
}

/// Resume strategy: handler failures do not trigger a restart.
/// on_start is called exactly once regardless of how many failures occur.
#[tokio::test]
async fn resume_strategy_does_not_restart() {
    // Given: a process with the Resume supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Resume,
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: multiple failing messages are sent
    let _ = proxy.tell(Fail).await;
    let _ = proxy.tell(Fail).await;
    let _ = proxy.tell(Fail).await;

    // Barrier: wait until all Fail messages have been processed
    let _ = proxy.ask(Ping).await;

    // Then: on_start was called exactly once (no restart occurred)
    assert_eq!(
        start_count.load(Ordering::SeqCst),
        1,
        "Resume must not restart on handler error"
    );
}

/// Resume strategy: on_stop is not called when a handler error occurs.
/// on_stop is only called when the process is explicitly stopped.
#[tokio::test]
async fn resume_strategy_does_not_call_on_stop_on_error() {
    // Given: a process with the Resume supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Resume,
    );
    let proxy = system.spawn(props).await;
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;

    // Barrier: ensure Fail has been processed before checking stop_count
    let _ = proxy.ask(Ping).await;

    // Then: on_stop has NOT been called (error was resumed, not stopped)
    assert_eq!(
        stop_count.load(Ordering::SeqCst),
        0,
        "Resume must not call on_stop on handler error"
    );

    // When: the process is explicitly stopped
    let _ = proxy.stop().await;

    // Then: on_stop is called exactly once (normal shutdown)
    wait_for_count(&stop_count, 1).await;
    assert_eq!(stop_count.load(Ordering::SeqCst), 1);
}

/// Resume strategy: the process remains registered in the registry after a handler failure.
#[tokio::test]
async fn resume_strategy_keeps_process_in_registry() {
    // Given: a process with the Resume supervision strategy
    let system = ProcessSystem::new().await;
    let start_count = Arc::new(AtomicU32::new(0));
    let stop_count = Arc::new(AtomicU32::new(0));
    let props = supervised_props(
        start_count.clone(),
        stop_count.clone(),
        SupervisionStrategy::Resume,
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_count(&start_count, 1).await;

    // When: a failing message is sent
    let _ = proxy.tell(Fail).await;

    // Barrier: ensure Fail has been processed
    let _ = proxy.ask(Ping).await;

    // Then: the process is still registered in the registry
    let found = system.lookup(pid).await;
    assert!(
        found.is_some(),
        "process with Resume strategy must remain in registry after handler error"
    );
}

/// Props::with_supervision_strategy is a builder method returning &mut Self,
/// enabling chained configuration.
#[tokio::test]
async fn props_supervision_strategy_builder_is_chainable() {
    // Given / When: builder-style configuration
    let (s, st, c) = (
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
    );
    let mut props = Props::new(move || SupervisedProcess {
        start_count: s.clone(),
        stop_count: st.clone(),
    });
    // Chain: with_supervision_strategy returns &mut Self
    let result = props.with_supervision_strategy(SupervisionStrategy::Restart {
        max_retries: 3,
        within: Duration::from_secs(60),
    });

    // Then: builder returns &mut Props (compiles and does not panic)
    let _ = result;
    let _ = c; // avoid unused warning
}
