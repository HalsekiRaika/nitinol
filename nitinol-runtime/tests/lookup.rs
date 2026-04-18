mod common;

use std::time::{Duration, Instant};

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;

use common::{test_props, tracked_state, wait_for_flag, Increment, TrackedProcess};

#[tokio::test]
async fn lookup_finds_spawned_process() {
    // Given: a spawned process
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_flag(&started).await;

    // When: lookup by Pid
    let found = system.lookup(pid).await;

    // Then: the process is found
    assert!(found.is_some());
}

#[tokio::test]
async fn lookup_returns_none_for_unknown_pid() {
    // Given: a process registered in one system
    let system1 = ProcessSystem::new();
    let system2 = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started, stopped, counter);
    let proxy = system1.spawn(props).await;
    let pid = proxy.pid();

    // When: lookup by that Pid in a different system where it's not registered
    let found = system2.lookup(pid).await;

    // Then: None is returned
    assert!(found.is_none());
}

#[tokio::test]
async fn lookup_by_name_finds_named_process() {
    // Given: a named process
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let name = ProcessName::new("named-worker");
    let _proxy = system.spawn_named(name.clone(), props).await;
    wait_for_flag(&started).await;

    // When: lookup by name
    let found = system.lookup_by_name(&name).await;

    // Then: the process is found
    assert!(found.is_some());
}

#[tokio::test]
async fn lookup_by_name_returns_none_for_unknown_name() {
    // Given: a ProcessSystem with no process named "ghost"
    let system = ProcessSystem::new();
    let unknown_name = ProcessName::new("ghost");

    // When: lookup by unknown name
    let found = system.lookup_by_name(&unknown_name).await;

    // Then: None is returned
    assert!(found.is_none());
}

#[tokio::test]
async fn lookup_returns_none_after_process_stops() {
    // Given: a spawned process that has been stopped
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_flag(&started).await;
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;

    // When: lookup after the process has stopped
    // (poll until the lifecycle task completes unregistration)
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = system.lookup(pid).await;
    while found.is_some() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for process to unregister"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
        found = system.lookup(pid).await;
    }

    // Then: the process is no longer registered
    assert!(found.is_none());
}

#[tokio::test]
async fn any_proxy_downcast_and_communicate() {
    // Given: a spawned process looked up via the registry
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped, counter);
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_flag(&started).await;

    let any_proxy = system
        .lookup(pid)
        .await
        .expect("process should be registered");
    let typed_proxy = any_proxy
        .downcast::<TrackedProcess>()
        .expect("downcast should succeed");

    // When: a message is sent through the downcasted proxy
    typed_proxy
        .tell(Increment)
        .await
        .expect("tell should succeed");

    // Then: the process receives the message (verify via the original proxy)
    let count = proxy
        .ask(common::GetCount)
        .await
        .expect("ask should succeed");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn any_proxy_stop_stops_process() {
    // Given: a spawned process looked up via the registry
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let _proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    let pid = _proxy.pid();
    let any_proxy = system
        .lookup(pid)
        .await
        .expect("process should be registered");

    // When: stop is called on AnyProxy
    any_proxy.stop().await.expect("stop should succeed");

    // Then: the process stops (on_stop is called)
    wait_for_flag(&stopped).await;
}
