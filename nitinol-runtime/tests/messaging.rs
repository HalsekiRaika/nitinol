#[path = "common/helpers.rs"]
mod common;

use std::time::Duration;

use nitinol_runtime::ProcessSystem;

use common::{test_props, tracked_state, wait_for_flag, FailingMessage, GetCount, Increment};

#[tokio::test]
async fn tell_delivers_message() {
    // Given: a spawned process with counter at 0
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: a tell message is sent
    proxy.tell(Increment).await.expect("tell should succeed");

    // Then: the message is processed (counter incremented)
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn tell_to_stopped_process_returns_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a tell message is sent to the stopped process
    let result = proxy.tell(Increment).await;

    // Then: an error is returned
    assert!(result.is_err());
}

#[tokio::test]
async fn ask_returns_response() {
    // Given: a spawned process with counter at 0
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: an ask message is sent
    let count = proxy.ask(GetCount).await.expect("ask should succeed");

    // Then: the handler's response is returned
    assert_eq!(count, 0);
}

#[tokio::test]
async fn ask_handler_error_returns_ask_error() {
    // Given: a spawned process
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: an ask message whose handler always fails is sent
    let result = proxy.ask(FailingMessage).await;

    // Then: an AskError is returned
    assert!(result.is_err());
}

#[tokio::test]
async fn ask_to_stopped_process_returns_error() {
    // Given: a process that has been stopped
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an ask message is sent to the stopped process
    let result = proxy.ask(GetCount).await;

    // Then: an error is returned
    assert!(result.is_err());
}

#[tokio::test]
async fn multiple_tells_processed_sequentially() {
    // Given: a spawned process with counter at 0
    let system = ProcessSystem::new().await;
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter.clone());
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: three tell messages are sent
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");

    // Then: all messages are processed in order (ask acts as a barrier)
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 3);
}
