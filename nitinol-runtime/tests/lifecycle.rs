mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use nitinol_runtime::ProcessSystem;

use common::{test_props, tracked_state, wait_for_flag};

#[tokio::test]
async fn on_start_called_when_process_spawns() {
    // Given: a ProcessSystem and tracked process
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);

    // When: process is spawned
    let _proxy = system.spawn(props).await;

    // Then: on_start is called
    wait_for_flag(&started).await;
}

#[tokio::test]
async fn stop_calls_on_stop() {
    // Given: a spawned process
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: process is stopped
    proxy.stop().await.expect("stop should succeed");

    // Then: on_stop is called
    wait_for_flag(&stopped).await;
}

#[tokio::test]
async fn poison_skips_on_stop() {
    // Given: a spawned process
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: process is poisoned
    proxy.poison().await.expect("poison should succeed");

    // Then: on_stop is NOT called
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn stop_on_stopped_process_returns_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    // Allow lifecycle task to fully exit and drop the receiver
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: stop is called again
    let result = proxy.stop().await;

    // Then: an error is returned (receiver already dropped)
    assert!(result.is_err());
}
