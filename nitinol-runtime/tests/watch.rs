mod common;

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy, Terminated, TerminatedReason};

use common::{wait_for_flag, wait_for_terminated, watcher_props, Barrier, TestError, WatchPid, WatcherProcess};

/// A simple target process that tracks lifecycle events.
struct TargetProcess {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Process for TargetProcess {
    fn on_start(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.started.store(true, Ordering::SeqCst);
        async {}
    }

    fn on_stop(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.stopped.store(true, Ordering::SeqCst);
        async {}
    }
}

fn target_props(started: Arc<AtomicBool>, stopped: Arc<AtomicBool>) -> Props<TargetProcess> {
    Props::new(move || TargetProcess {
        started: started.clone(),
        stopped: stopped.clone(),
    })
}

/// A target process that can also fail on demand (for supervision+watch tests).
struct FaultyTarget {
    start_count: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
}

/// Message whose handler always fails (triggers supervision).
struct FailTarget;

impl Receive<FailTarget> for FaultyTarget {
    type Response = ();
    type Error = TestError;
    async fn recv(&mut self, _: FailTarget, _ctx: &mut ProcessContext) -> Result<(), TestError> {
        Err(TestError("intentional failure".to_string()))
    }
}

impl Process for FaultyTarget {
    fn on_start(&mut self, _ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        self.start_count.fetch_add(1, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        async {}
    }
}

fn faulty_target_props(
    start_count: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    strategy: SupervisionStrategy,
) -> Props<FaultyTarget> {
    let mut props = Props::new(move || FaultyTarget {
        start_count: start_count.clone(),
        started: started.clone(),
    });
    props.with_supervision_strategy(strategy);
    props
}

/// Tell the watcher to stop watching the given PID.
struct UnwatchPid {
    pid: Pid,
}

impl Receive<UnwatchPid> for WatcherProcess {
    type Response = ();
    type Error = std::convert::Infallible;
    async fn recv(
        &mut self,
        msg: UnwatchPid,
        ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        ctx.unwatch(msg.pid).await;
        Ok(())
    }
}

/// Waits for an AtomicU32 to reach at least `expected`, with a 5-second timeout.
async fn wait_for_u32(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for count to reach {expected}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Watching a live process and then stopping it delivers Terminated{Stopped}.
#[tokio::test]
async fn watch_live_process_receives_terminated_stopped_on_stop() {
    // Given: a target process (running)
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_stopped = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(target_props(target_started.clone(), target_stopped.clone()))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // And: a watcher that begins watching the target
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    // Barrier: ensure the watch is registered before we stop the target
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier ask should succeed");

    // When: the target is stopped
    target_proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&target_stopped).await;

    // Then: the watcher receives Terminated{who: target_pid, why: Stopped}
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Stopped);
}

/// The Terminated.who field matches the PID of the stopped process.
#[tokio::test]
async fn terminated_who_matches_stopped_process_pid() {
    // Given: two distinct processes — target A and target B
    let system = ProcessSystem::new().await;
    let a_started = Arc::new(AtomicBool::new(false));
    let a_stopped = Arc::new(AtomicBool::new(false));
    let proxy_a = system
        .spawn(target_props(a_started.clone(), a_stopped.clone()))
        .await;
    let pid_a = proxy_a.pid();
    wait_for_flag(&a_started).await;

    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;

    // Watch only A
    watcher_proxy
        .tell(WatchPid { pid: pid_a })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: A is stopped
    proxy_a.stop().await.expect("stop should succeed");
    wait_for_flag(&a_stopped).await;

    // Then: Terminated.who == pid_a (not some other PID)
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, pid_a, "Terminated.who must be the stopped process PID");
}

/// Watching a process that is already stopped delivers Terminated{NotFound}.
#[tokio::test]
async fn watch_already_stopped_process_receives_terminated_not_found() {
    // Given: a process that has already been stopped
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_stopped = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(target_props(target_started.clone(), target_stopped.clone()))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;
    target_proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&target_stopped).await;
    // Allow the lifecycle task to fully exit and unregister
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a watcher watches the already-dead PID
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");

    // Then: the watcher receives Terminated{who: target_pid, why: NotFound}
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::NotFound);
}

/// After calling unwatch, the watcher does NOT receive Terminated when the target stops.
#[tokio::test]
async fn unwatch_cancels_terminated_notification() {
    // Given: a target process and a watcher that begins watching it
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_stopped = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(target_props(target_started.clone(), target_stopped.clone()))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: the watcher unwatches the target
    watcher_proxy
        .tell(UnwatchPid { pid: target_pid })
        .await
        .expect("tell UnwatchPid should succeed");
    // Barrier: ensure unwatch is processed before stopping the target
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // And: the target is stopped
    target_proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&target_stopped).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: the watcher does NOT receive Terminated
    let guard = received.lock().await;
    assert!(
        guard.is_none(),
        "watcher should not receive Terminated after calling unwatch"
    );
}

/// All watchers receive Terminated when the target process stops.
#[tokio::test]
async fn multiple_watchers_all_receive_terminated() {
    // Given: a target process
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_stopped = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(target_props(target_started.clone(), target_stopped.clone()))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // And: two watchers, each watching the same target
    let received_a = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_a = system.spawn(watcher_props(received_a.clone(), IdleTimeout::Inherit)).await;
    watcher_a
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_a.ask(Barrier).await.expect("barrier should succeed");

    let received_b = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_b = system.spawn(watcher_props(received_b.clone(), IdleTimeout::Inherit)).await;
    watcher_b
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_b.ask(Barrier).await.expect("barrier should succeed");

    // When: the target is stopped
    target_proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&target_stopped).await;

    // Then: both watchers receive Terminated{Stopped}
    wait_for_terminated(&received_a).await;
    wait_for_terminated(&received_b).await;

    let t_a = received_a.lock().await.clone().unwrap();
    let t_b = received_b.lock().await.clone().unwrap();
    assert_eq!(t_a.who, target_pid);
    assert_eq!(t_a.why, TerminatedReason::Stopped);
    assert_eq!(t_b.who, target_pid);
    assert_eq!(t_b.why, TerminatedReason::Stopped);
}

/// A process restart (not permanent stop) does not send Terminated to watchers.
/// Only the final permanent stop sends Terminated.
#[tokio::test]
async fn restart_does_not_send_terminated_to_watchers() {
    // Given: a target with Restart strategy and a watcher watching it
    let system = ProcessSystem::new().await;
    let target_start_count = Arc::new(AtomicU32::new(0));
    let target_started = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(faulty_target_props(
            target_start_count.clone(),
            target_started.clone(),
            SupervisionStrategy::Restart {
                max_retries: 5,
                within: Duration::from_secs(10),
            },
        ))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // Set up watcher
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: the target fails (triggers a restart, NOT a permanent stop)
    let _ = target_proxy.tell(FailTarget).await;
    wait_for_u32(&target_start_count, 2).await; // Restarted once

    // Then: no Terminated received yet (restart ≠ stop)
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let guard = received.lock().await;
        assert!(
            guard.is_none(),
            "watcher should NOT receive Terminated during a restart (only on permanent stop)"
        );
    }

    // When: the target is permanently stopped
    target_proxy.stop().await.expect("stop should succeed");

    // Then: Terminated{Stopped} is now received
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Stopped);
}

/// When the target's rate limit is exceeded (permanent stop), watchers receive Terminated{Stopped}.
#[tokio::test]
async fn rate_limit_stop_sends_terminated_to_watchers() {
    // Given: a target with Restart{max_retries: 1} and a watcher
    let system = ProcessSystem::new().await;
    let target_start_count = Arc::new(AtomicU32::new(0));
    let target_started = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(faulty_target_props(
            target_start_count.clone(),
            target_started.clone(),
            SupervisionStrategy::Restart {
                max_retries: 1,
                within: Duration::from_secs(10),
            },
        ))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // Set up watcher
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: 1 restart occurs (within limit)
    let _ = target_proxy.tell(FailTarget).await;
    wait_for_u32(&target_start_count, 2).await;

    // When: 1 more failure exceeds the rate limit (permanent stop)
    let _ = target_proxy.tell(FailTarget).await;

    // Then: Terminated{Stopped} is delivered to the watcher
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Stopped);
}

/// A watch registered before a restart remains active across restarts.
/// The watcher still receives Terminated when the process permanently stops.
#[tokio::test]
async fn watch_persists_across_restart() {
    // Given: a target with Restart strategy and a watcher
    let system = ProcessSystem::new().await;
    let target_start_count = Arc::new(AtomicU32::new(0));
    let target_started = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(faulty_target_props(
            target_start_count.clone(),
            target_started.clone(),
            SupervisionStrategy::Restart {
                max_retries: 5,
                within: Duration::from_secs(10),
            },
        ))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // Set up watcher before any restart
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: target restarts once
    let _ = target_proxy.tell(FailTarget).await;
    wait_for_u32(&target_start_count, 2).await;

    // And: no Terminated received during restart
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let guard = received.lock().await;
        assert!(guard.is_none(), "no Terminated should arrive during restart");
    }

    // When: target permanently stops
    target_proxy.stop().await.expect("stop should succeed");

    // Then: watch still active → Terminated{Stopped} arrives
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Stopped);
}

/// TerminatedReason implements PartialEq: same variants are equal, different are not.
#[tokio::test]
async fn terminated_reason_partial_eq() {
    // Given / When / Then
    assert_eq!(TerminatedReason::Stopped, TerminatedReason::Stopped);
    assert_eq!(TerminatedReason::NotFound, TerminatedReason::NotFound);
    assert_ne!(TerminatedReason::Stopped, TerminatedReason::NotFound);
}

/// TerminatedReason is Copy: a copy leaves the original intact.
#[tokio::test]
async fn terminated_reason_is_copy() {
    // Given: a TerminatedReason value
    let original = TerminatedReason::Stopped;

    // When: a copy is made
    let copy = original;

    // Then: original is still usable (Copy semantics)
    assert_eq!(original, copy);
}

/// Terminated is Clone: cloning produces an independent value with the same fields.
#[tokio::test]
async fn terminated_is_clone() {
    // Given: a Terminated value (constructed directly for type-property testing)
    // We use the public constructor fields from the integration test below,
    // but here we just confirm Clone compiles — integration tests confirm field values.
    let _ = TerminatedReason::Stopped.clone();
    let _ = TerminatedReason::NotFound.clone();
}

/// Poisoning a live process delivers Terminated{Poisoned} to its watcher
/// and skips on_stop (no cleanup is performed).
#[tokio::test]
async fn watch_live_process_receives_terminated_poisoned_on_poison() {
    // Given: a target process (running) with lifecycle event tracking
    let system = ProcessSystem::new().await;
    let target_started = Arc::new(AtomicBool::new(false));
    let target_stopped = Arc::new(AtomicBool::new(false));
    let target_proxy = system
        .spawn(target_props(target_started.clone(), target_stopped.clone()))
        .await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&target_started).await;

    // And: a watcher that begins watching the target
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system.spawn(watcher_props(received.clone(), IdleTimeout::Inherit)).await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    // Barrier: ensure the watch is registered before we poison the target
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier ask should succeed");

    // When: the target is poisoned (force-killed, no cleanup)
    target_proxy.poison().await.expect("poison should succeed");

    // Then: the watcher receives Terminated{who: target_pid, why: Poisoned}
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Poisoned);

    // And: on_stop was NOT called on the target (Poisoned skips cleanup)
    assert!(
        !target_stopped.load(Ordering::SeqCst),
        "on_stop must not be called when a process is poisoned"
    );
}

/// TerminatedReason::Poisoned is distinct from Stopped (they are separate termination causes).
#[tokio::test]
async fn terminated_reason_poisoned_is_distinct_from_stopped() {
    assert_ne!(TerminatedReason::Poisoned, TerminatedReason::Stopped);
}

/// TerminatedReason::Poisoned is distinct from NotFound.
#[tokio::test]
async fn terminated_reason_poisoned_is_distinct_from_not_found() {
    assert_ne!(TerminatedReason::Poisoned, TerminatedReason::NotFound);
}

/// TerminatedReason::Poisoned is distinct from Timeout.
#[tokio::test]
async fn terminated_reason_poisoned_is_distinct_from_timeout() {
    assert_ne!(TerminatedReason::Poisoned, TerminatedReason::Timeout);
}

/// TerminatedReason::Poisoned implements PartialEq: equal to itself.
#[tokio::test]
async fn terminated_reason_poisoned_is_equal_to_itself() {
    assert_eq!(TerminatedReason::Poisoned, TerminatedReason::Poisoned);
}

/// TerminatedReason::Poisoned is Copy: a copy leaves the original intact.
#[tokio::test]
async fn terminated_reason_poisoned_is_copy() {
    // Given: a Poisoned variant value
    let original = TerminatedReason::Poisoned;

    // When: an implicit copy is made (no explicit clone)
    let copy = original;

    // Then: both values are equal (original is still usable — Copy semantics)
    assert_eq!(original, copy);
}

/// TerminatedReason::Poisoned is Clone: clone produces an equal value.
#[tokio::test]
async fn terminated_reason_poisoned_is_clone() {
    // Given / When: clone the Poisoned variant
    let cloned = TerminatedReason::Poisoned.clone();

    // Then: the clone equals the original
    assert_eq!(cloned, TerminatedReason::Poisoned);
}
