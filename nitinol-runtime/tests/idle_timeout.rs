mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{IdleTimeout, ProcessSystem, Terminated, TerminatedReason};

use common::{
    test_props, tracked_state, wait_for_flag, wait_for_terminated, watcher_props, Barrier,
    Increment, WatchPid,
};

// ============================================================
// Type-property tests
// ============================================================

/// TerminatedReason::Timeout is a valid variant: PartialEq with itself.
#[tokio::test]
async fn terminated_reason_timeout_is_equal_to_itself() {
    // Given / When / Then
    assert_eq!(TerminatedReason::Timeout, TerminatedReason::Timeout);
}

/// TerminatedReason::Timeout is distinct from Stopped.
#[tokio::test]
async fn terminated_reason_timeout_is_distinct_from_stopped() {
    assert_ne!(TerminatedReason::Timeout, TerminatedReason::Stopped);
}

/// TerminatedReason::Timeout is distinct from NotFound.
#[tokio::test]
async fn terminated_reason_timeout_is_distinct_from_not_found() {
    assert_ne!(TerminatedReason::Timeout, TerminatedReason::NotFound);
}

/// TerminatedReason::Timeout is Copy: a copy leaves the original value intact.
#[tokio::test]
async fn terminated_reason_timeout_is_copy() {
    // Given: a Timeout variant value
    let original = TerminatedReason::Timeout;

    // When: an implicit copy is made (no explicit clone)
    let copy = original;

    // Then: both values are equal (original is still usable — Copy semantics)
    assert_eq!(original, copy);
}

/// TerminatedReason::Timeout is Clone: clone produces an equal value.
#[tokio::test]
async fn terminated_reason_timeout_is_clone() {
    // Given / When: clone the Timeout variant
    let cloned = TerminatedReason::Timeout.clone();

    // Then: the clone equals the original
    assert_eq!(cloned, TerminatedReason::Timeout);
}

/// IdleTimeout::Inherit is the Default value.
#[tokio::test]
async fn idle_timeout_default_is_inherit() {
    // Given / When
    let default: IdleTimeout = IdleTimeout::default();

    // Then
    assert!(
        matches!(default, IdleTimeout::Inherit),
        "IdleTimeout::default() must be Inherit"
    );
}

/// IdleTimeout::Persistent variant exists and can be matched.
#[tokio::test]
async fn idle_timeout_persistent_variant_exists() {
    // Given / When
    let persistent = IdleTimeout::Persistent;

    // Then
    assert!(matches!(persistent, IdleTimeout::Persistent));
}

/// IdleTimeout::After(d) holds the provided Duration.
#[tokio::test]
async fn idle_timeout_after_holds_duration() {
    // Given / When
    let dur = Duration::from_secs(42);
    let timeout = IdleTimeout::After(dur);

    // Then: the inner Duration is the one that was supplied
    match timeout {
        IdleTimeout::After(d) => assert_eq!(d, dur),
        _ => panic!("expected IdleTimeout::After"),
    }
}

/// Props::with_idle_timeout returns `&mut Self`, enabling builder chaining.
#[tokio::test]
async fn props_with_idle_timeout_builder_is_chainable() {
    // Given: freshly created Props
    let (started, stopped, counter) = tracked_state();
    let mut props = test_props(started, stopped, counter);

    // When: with_idle_timeout is called as a builder method
    let result = props.with_idle_timeout(IdleTimeout::After(Duration::from_secs(10)));

    // Then: the return type is &mut Props (compiles; no panic)
    let _ = result;
}

/// ProcessSystem::with_default_idle_timeout consumes self and returns Self,
/// allowing method chaining after ProcessSystem::new().await.
#[tokio::test]
async fn process_system_with_default_idle_timeout_is_consuming_builder() {
    // Given: a new ProcessSystem
    let system = ProcessSystem::new().await;

    // When: with_default_idle_timeout is called (consuming builder — takes ownership)
    let _system = system.with_default_idle_timeout(Duration::from_secs(30));

    // Then: method compiles and returns ProcessSystem; no panic
}

// ============================================================
// Integration tests
// ============================================================

/// A process configured with Props::After(d) calls on_stop after the idle period.
///
/// This confirms that the timeout Duration in Props reaches lifecycle_loop and
/// fires, triggering the normal shutdown path.
#[tokio::test]
async fn idle_timeout_after_calls_on_stop_when_idle() {
    // Given: a process with a 50ms idle timeout
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let mut props = test_props(started.clone(), stopped.clone(), counter);
    props.with_idle_timeout(IdleTimeout::After(Duration::from_millis(50)));

    // When: the process is spawned and no messages are sent
    let _proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // Then: on_stop is called (idle timeout fires and terminates the process)
    wait_for_flag(&stopped).await;
}

/// A process configured with Props::After(d) notifies its watcher with
/// TerminatedReason::Timeout — not Stopped — when the idle timer fires.
#[tokio::test]
async fn idle_timeout_after_notifies_watcher_with_timeout_reason() {
    // Given: a target process with a 50ms idle timeout
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let mut target_props = test_props(started.clone(), stopped.clone(), counter);
    target_props.with_idle_timeout(IdleTimeout::After(Duration::from_millis(50)));
    let target_proxy = system.spawn(target_props).await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&started).await;

    // And: a watcher watching the target
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system
        .spawn(watcher_props(received.clone(), IdleTimeout::Persistent))
        .await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    // Barrier: ensure watch is registered before the timeout fires
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: no messages are sent (idle timeout fires)

    // Then: the watcher receives Terminated with who=target_pid and why=Timeout
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Timeout);
}

/// Props::Inherit + ProcessSystem::with_default_idle_timeout: the process times out
/// using the system default duration, and the watcher receives Timeout.
#[tokio::test]
async fn idle_timeout_inherit_uses_system_default_and_notifies_with_timeout() {
    // Given: a system with a 50ms default idle timeout
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_millis(50));

    // And: a process whose idle timeout is Inherit (the default — no explicit call)
    let (started, stopped, counter) = tracked_state();
    let target_props = test_props(started.clone(), stopped.clone(), counter);
    let target_proxy = system.spawn(target_props).await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&started).await;

    // And: a watcher registered before any timeout fires
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system
        .spawn(watcher_props(received.clone(), IdleTimeout::Persistent))
        .await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: no messages are sent (system default fires)

    // Then: watcher receives Terminated{why: Timeout}
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(terminated.who, target_pid);
    assert_eq!(terminated.why, TerminatedReason::Timeout);
}

/// Props::Persistent overrides the system default:
/// the process does NOT time out even when the system has a short idle timeout.
/// An explicit stop delivers TerminatedReason::Stopped, not Timeout.
#[tokio::test]
async fn idle_timeout_persistent_ignores_system_default() {
    // Given: a system with a 50ms default idle timeout
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_millis(50));

    // And: a process explicitly set to Persistent
    let (started, stopped, counter) = tracked_state();
    let mut target_props = test_props(started.clone(), stopped.clone(), counter);
    target_props.with_idle_timeout(IdleTimeout::Persistent);
    let target_proxy = system.spawn(target_props).await;
    let target_pid = target_proxy.pid();
    wait_for_flag(&started).await;

    // And: a watcher
    let received = Arc::new(Mutex::new(None::<Terminated>));
    let watcher_proxy = system
        .spawn(watcher_props(received.clone(), IdleTimeout::Persistent))
        .await;
    watcher_proxy
        .tell(WatchPid { pid: target_pid })
        .await
        .expect("tell WatchPid should succeed");
    watcher_proxy
        .ask(Barrier)
        .await
        .expect("barrier should succeed");

    // When: the system default timeout period passes (150ms >> 50ms default)
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Then: the Persistent process has NOT been stopped by idle timeout
    assert!(
        !stopped.load(Ordering::SeqCst),
        "Persistent process must not stop due to system idle timeout"
    );

    // And: an explicit stop delivers Terminated{Stopped}, not Timeout
    target_proxy
        .stop()
        .await
        .expect("explicit stop should succeed");
    wait_for_terminated(&received).await;
    let terminated = received.lock().await.clone().unwrap();
    assert_eq!(
        terminated.why,
        TerminatedReason::Stopped,
        "explicitly stopped Persistent process must report Stopped, not Timeout"
    );
}

/// Props::Inherit with no system default resolves to persistent:
/// the process stays alive indefinitely when no timeout is configured anywhere.
#[tokio::test]
async fn idle_timeout_inherit_with_no_system_default_is_persistent() {
    // Given: a system without a default idle timeout (initial state)
    let system = ProcessSystem::new().await;

    // And: a process with the default idle timeout (Inherit, which resolves to None)
    let (started, stopped, counter) = tracked_state();
    let target_props = test_props(started.clone(), stopped.clone(), counter);
    // No with_idle_timeout call: Props::default() is Inherit; no system default → persistent

    let _proxy = system.spawn(target_props).await;
    wait_for_flag(&started).await;

    // When: a long idle period passes with no messages
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then: the process has not been stopped (it is persistent)
    assert!(
        !stopped.load(Ordering::SeqCst),
        "process with Inherit + no system default must remain persistent"
    );
}

/// Props::After(d1) overrides the system default timeout d2 (where d2 >> d1):
/// the process uses its own short timeout and stops, ignoring the longer system default.
#[tokio::test]
async fn idle_timeout_after_overrides_longer_system_default() {
    // Given: a system with a 60-second default (effectively infinite for this test)
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_secs(60));

    // And: a process with its own 50ms timeout (much shorter than the system default)
    let (started, stopped, counter) = tracked_state();
    let mut target_props = test_props(started.clone(), stopped.clone(), counter);
    target_props.with_idle_timeout(IdleTimeout::After(Duration::from_millis(50)));

    let _proxy = system.spawn(target_props).await;
    wait_for_flag(&started).await;

    // When: no messages are sent

    // Then: the process stops at its own 50ms Props timeout (not the 60s system default)
    wait_for_flag(&stopped).await;
}

/// Receiving a message resets the idle timer: the timeout fires 100ms after the
/// LAST message, not 100ms after the process started.
#[tokio::test]
async fn idle_message_resets_timeout_timer() {
    // Given: a process with a 100ms idle timeout
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let mut target_props = test_props(started.clone(), stopped.clone(), counter);
    target_props.with_idle_timeout(IdleTimeout::After(Duration::from_millis(100)));
    let target_proxy = system.spawn(target_props).await;
    wait_for_flag(&started).await;

    // When: a message is sent at ~60ms (before the original 100ms timeout fires)
    tokio::time::sleep(Duration::from_millis(60)).await;
    target_proxy
        .tell(Increment)
        .await
        .expect("tell should succeed");

    // Then: at ~130ms from start (60ms + 70ms), the process is still alive.
    // Original timer would have fired at 100ms, but the message reset it to 60ms+100ms=160ms.
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(
        !stopped.load(Ordering::SeqCst),
        "process should still be alive: timer was reset by the message sent at ~60ms"
    );

    // And: the reset timer fires ~100ms after the message (at ~160ms from start)
    wait_for_flag(&stopped).await;
}

/// The `$dead-letters` stream process is NOT affected by the system default idle timeout;
/// it remains registered and alive after the system default timeout period elapses.
#[tokio::test]
async fn dead_letter_stream_survives_system_default_idle_timeout() {
    // Given: a system with a 50ms default idle timeout
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_millis(50));

    // When: the timeout period passes with no messages sent to $dead-letters
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then: the $dead-letters stream process is still registered in the registry
    let dead_letters_name = ProcessName::new("$dead-letters");
    let found = system.lookup_by_name(&dead_letters_name).await;
    assert!(
        found.is_some(),
        "$dead-letters stream must remain alive regardless of system default idle timeout"
    );
}

/// The `$dead-letter` actor process is NOT affected by the system default idle timeout;
/// it remains registered and alive after the system default timeout period elapses.
#[tokio::test]
async fn dead_letter_process_survives_system_default_idle_timeout() {
    // Given: a system with a 50ms default idle timeout
    let system = ProcessSystem::new()
        .await
        .with_default_idle_timeout(Duration::from_millis(50));

    // When: the timeout period passes with no messages sent to $dead-letter
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then: the $dead-letter actor process is still registered in the registry
    let dead_letter_name = ProcessName::new("$dead-letter");
    let found = system.lookup_by_name(&dead_letter_name).await;
    assert!(
        found.is_some(),
        "$dead-letter actor must remain alive regardless of system default idle timeout"
    );
}
