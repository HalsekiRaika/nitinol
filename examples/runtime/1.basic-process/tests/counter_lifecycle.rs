//! Integration tests for the Counter process.
//!
//! These tests verify that Counter correctly satisfies the Process trait and
//! that its lifecycle (on_start / on_stop) integrates with ProcessSystem.

use std::time::Duration;

use basic_process::counter::Counter;
use nitinol_runtime::{ProcessSystem, Props};

// ─── Counter::new ─────────────────────────────────────────────────────────────

#[test]
fn counter_new_with_zero_does_not_panic() {
    // Given / When: create a counter starting at zero
    let _counter = Counter::new(0);
    // Then: no panic
}

#[test]
fn counter_new_with_nonzero_does_not_panic() {
    // Given / When: create a counter with an arbitrary initial value
    let _counter = Counter::new(42);
    // Then: no panic
}

#[test]
fn counter_new_with_max_u32_does_not_panic() {
    // Given / When: boundary check at the maximum representable count
    let _counter = Counter::new(u32::MAX);
    // Then: no panic
}

// ─── Process lifecycle via ProcessSystem ──────────────────────────────────────

#[tokio::test]
async fn counter_spawns_and_returns_proxy() {
    // Given: an initialised ProcessSystem
    let system = ProcessSystem::new().await;
    let props = Props::new(|| Counter::new(0));

    // When: counter is spawned
    let proxy = system.spawn(props).await;

    // Then: proxy is returned immediately; on_start runs asynchronously inside the task
    let _ = proxy.pid();
}

#[tokio::test]
async fn each_spawned_counter_has_distinct_pid() {
    // Given: a ProcessSystem
    let system = ProcessSystem::new().await;

    // When: two counters are spawned
    let proxy1 = system.spawn(Props::new(|| Counter::new(0))).await;
    let proxy2 = system.spawn(Props::new(|| Counter::new(0))).await;

    // Then: PIDs are unique — each spawn owns its own process identity
    assert_ne!(proxy1.pid(), proxy2.pid());
}

#[tokio::test]
async fn counter_stop_succeeds() {
    // Given: a spawned counter whose on_start has completed
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(10))).await;
    // on_start runs asynchronously; wait before issuing stop so the task is running
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: stop is requested via the proxy
    let result = proxy.stop().await;

    // Then: stop succeeds and on_stop runs inside the task
    assert!(result.is_ok());
}

#[tokio::test]
async fn counter_stop_twice_returns_error() {
    // Given: a counter that has already been stopped
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(5))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy.stop().await.expect("first stop should succeed");
    // Allow the lifecycle task to fully exit and drop the sys_tx receiver
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: stop is requested again
    let result = proxy.stop().await;

    // Then: SendError because the channel receiver has been dropped
    assert!(result.is_err());
}

#[tokio::test]
async fn multiple_counters_spawn_as_independent_processes() {
    // Given: three counters with different initial values
    let system = ProcessSystem::new().await;
    let proxy_a = system.spawn(Props::new(|| Counter::new(1))).await;
    let proxy_b = system.spawn(Props::new(|| Counter::new(99))).await;
    let proxy_c = system.spawn(Props::new(|| Counter::new(0))).await;

    // When / Then: all three PIDs are distinct — they are separate tokio tasks
    assert_ne!(proxy_a.pid(), proxy_b.pid());
    assert_ne!(proxy_b.pid(), proxy_c.pid());
    assert_ne!(proxy_a.pid(), proxy_c.pid());

    // Cleanup
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy_a.stop().await.ok();
    proxy_b.stop().await.ok();
    proxy_c.stop().await.ok();
}
