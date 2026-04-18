mod common;

use std::sync::atomic::Ordering;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::AnyProxy;
use nitinol_runtime::ProcessSystem;

use common::{test_props, tracked_state, wait_for_flag, GetCount, Increment, TrackedProcess};

#[tokio::test]
async fn full_tell_ask_lifecycle_flow() {
    // Given: a spawned process
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter.clone());
    let proxy = system.spawn(props).await;
    wait_for_flag(&started).await;

    // When: tell messages are sent, then ask, then stop
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;

    // Then: messages were processed and lifecycle completed
    assert_eq!(count, 2);
    assert!(stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn named_process_lookup_and_communicate() {
    // Given: a named process
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let name = ProcessName::new("worker");
    let proxy = system.spawn_named(name.clone(), props).await;
    wait_for_flag(&started).await;

    // When: the process is found by name and communicated with
    let any_proxy = system
        .lookup_by_name(&name)
        .await
        .expect("process should be registered");
    let found_proxy = any_proxy
        .downcast::<TrackedProcess>()
        .expect("downcast should succeed");
    found_proxy
        .tell(Increment)
        .await
        .expect("tell should succeed");

    // Then: the message is received by the same process
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 1);

    proxy.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
}

#[tokio::test]
async fn multiple_processes_maintain_independent_state() {
    // Given: two independent processes
    let system = ProcessSystem::new();
    let (started1, stopped1, counter1) = tracked_state();
    let (started2, stopped2, counter2) = tracked_state();
    let props1 = test_props(started1.clone(), stopped1.clone(), counter1);
    let props2 = test_props(started2.clone(), stopped2.clone(), counter2);
    let proxy1 = system.spawn(props1).await;
    let proxy2 = system.spawn(props2).await;
    wait_for_flag(&started1).await;
    wait_for_flag(&started2).await;

    // When: different numbers of messages are sent to each
    proxy1.tell(Increment).await.expect("tell should succeed");
    proxy1.tell(Increment).await.expect("tell should succeed");
    proxy2.tell(Increment).await.expect("tell should succeed");

    // Then: each process maintains its own state
    let count1 = proxy1.ask(GetCount).await.expect("ask should succeed");
    let count2 = proxy2.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count1, 2);
    assert_eq!(count2, 1);

    proxy1.stop().await.expect("stop should succeed");
    proxy2.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped1).await;
    wait_for_flag(&stopped2).await;
}

/// Regression test for internal-api-leak (AIR-001):
/// AnyProxy is the only type-erased handle exposed publicly.
/// The internal DynProxy trait must remain pub(crate) and must NOT
/// be required by external consumers. This test proves AnyProxy's
/// public API (downcast, stop) works from an external crate without
/// referencing any internal trait.
#[tokio::test]
async fn any_proxy_public_api_does_not_require_internal_traits() {
    let system = ProcessSystem::new();
    let (started, stopped, counter) = tracked_state();
    let props = test_props(started.clone(), stopped.clone(), counter);
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    wait_for_flag(&started).await;

    // Obtain AnyProxy through the public registry API
    let any: AnyProxy = system
        .lookup(pid)
        .await
        .expect("process should be registered");

    // downcast back to a typed proxy — public API only
    let typed = any
        .downcast::<TrackedProcess>()
        .expect("downcast should succeed");
    typed.tell(Increment).await.expect("tell should succeed");
    let count = typed.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 1);

    // stop via AnyProxy — public API only
    let any2: AnyProxy = system
        .lookup(pid)
        .await
        .expect("process should be registered");
    any2.stop().await.expect("stop should succeed");
    wait_for_flag(&stopped).await;
}
