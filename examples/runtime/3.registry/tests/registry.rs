//! Integration tests for the registry example.
//!
//! These tests verify:
//! - `spawn_named` registers a process under a name and returns a usable proxy
//! - `lookup(Pid)` finds a process by its runtime-assigned identifier
//! - `lookup_by_name(ProcessName)` finds a process by its stable name
//! - `AnyProxy::downcast` recovers a typed proxy for the correct type
//! - `AnyProxy::downcast` returns `None` for a mismatched type

use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::AnyProxy;
use nitinol_runtime::{ProcessSystem, Props};

use registry::counter::{Counter, Decrement, GetCount, Increment};
use registry::greeter::{Greet, Greeter};

// ─── spawn_named: returned proxy is operational ───────────────────────────────

#[tokio::test]
async fn spawn_named_returns_usable_proxy() {
    // Given: a ProcessSystem and a process name
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("counter");

    // When: a Counter is spawned with that name
    let proxy = system
        .spawn(Props::new(|| Counter::new(0)).with_name(name))
        .await;
    // on_start runs asynchronously; wait before sending messages
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the returned proxy can send and receive messages
    proxy.tell(Increment).await.expect("tell should succeed");
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 1);

    proxy.stop().await.ok();
}

// ─── lookup_by_name ───────────────────────────────────────────────────────────

#[tokio::test]
async fn lookup_by_name_finds_named_process() {
    // Given: a Counter registered under "counter"
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("counter");
    let _proxy = system
        .spawn(Props::new(|| Counter::new(0)).with_name(name.clone()))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the process is looked up by name
    let found = system.lookup_by_name(&name).await;

    // Then: the process is found
    assert!(found.is_some());
}

#[tokio::test]
async fn lookup_by_name_returns_none_for_unknown() {
    // Given: a ProcessSystem with no registered processes
    let system = ProcessSystem::new().await;
    let unknown = ProcessName::new("unknown-service");

    // When: an unregistered name is looked up
    let found = system.lookup_by_name(&unknown).await;

    // Then: None is returned — unknown names are not fabricated
    assert!(found.is_none());
}

// ─── lookup by Pid ────────────────────────────────────────────────────────────

#[tokio::test]
async fn lookup_by_pid_finds_process() {
    // Given: a Greeter process registered under "greeter"
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("greeter");
    let proxy = system.spawn(Props::new(|| Greeter).with_name(name)).await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the process is looked up by its runtime-assigned Pid
    let found = system.lookup(pid).await;

    // Then: the process is found — Pid is a valid handle within its lifetime
    assert!(found.is_some());

    proxy.stop().await.ok();
}

// ─── AnyProxy::downcast: type recovery ───────────────────────────────────────

#[tokio::test]
async fn downcast_to_correct_type_succeeds() {
    // Given: a Counter looked up as a type-erased AnyProxy
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("counter");
    let _proxy = system
        .spawn(Props::new(|| Counter::new(0)).with_name(name.clone()))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let any_proxy: AnyProxy = system
        .lookup_by_name(&name)
        .await
        .expect("process should be registered");

    // When: the AnyProxy is downcast to the correct type
    let typed = any_proxy.downcast::<Counter>();

    // Then: downcast succeeds and returns a usable typed proxy
    assert!(typed.is_some());
}

#[tokio::test]
async fn downcast_to_wrong_type_returns_none() {
    // Given: a Counter's AnyProxy
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("counter");
    let _proxy = system
        .spawn(Props::new(|| Counter::new(0)).with_name(name.clone()))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counter_any: AnyProxy = system
        .lookup_by_name(&name)
        .await
        .expect("process should be registered");

    // When: the Counter's AnyProxy is downcast to Greeter (wrong type)
    let typed = counter_any.downcast::<Greeter>();

    // Then: None is returned — type mismatch is detected at runtime
    assert!(typed.is_none());
}

// ─── End-to-end: discovered Counter communication ────────────────────────────

#[tokio::test]
async fn discovered_counter_can_communicate() {
    // Given: a Counter registered under "counter"
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("counter");
    let _proxy = system
        .spawn(Props::new(|| Counter::new(0)).with_name(name.clone()))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the Counter is discovered by name, downcast, then mutated and queried
    let any_proxy = system
        .lookup_by_name(&name)
        .await
        .expect("process should be registered");
    let counter = any_proxy
        .downcast::<Counter>()
        .expect("downcast to Counter should succeed");
    counter.tell(Increment).await.expect("tell should succeed");
    counter.tell(Increment).await.expect("tell should succeed");
    counter.tell(Decrement).await.expect("tell should succeed");
    let count = counter.ask(GetCount).await.expect("ask should succeed");

    // Then: the state reflects all mutations (2 increments − 1 decrement = 1)
    assert_eq!(count, 1);

    counter.stop().await.ok();
}

// ─── End-to-end: discovered Greeter communication ────────────────────────────

#[tokio::test]
async fn discovered_greeter_can_communicate() {
    // Given: a Greeter registered under "greeter"
    let system = ProcessSystem::new().await;
    let name = ProcessName::new("greeter");
    let proxy = system
        .spawn(Props::new(|| Greeter).with_name(name.clone()))
        .await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: the Greeter is discovered by Pid, downcast, then asked
    let any_proxy = system
        .lookup(pid)
        .await
        .expect("process should be registered");
    let greeter = any_proxy
        .downcast::<Greeter>()
        .expect("downcast to Greeter should succeed");
    let greeting = greeter
        .ask(Greet("World".to_string()))
        .await
        .expect("ask should succeed");

    // Then: the greeting matches the expected format
    assert_eq!(greeting, "Hello, World!");

    proxy.stop().await.ok();
}

// ─── Multiple named processes coexist ────────────────────────────────────────

#[tokio::test]
async fn multiple_named_processes_coexist() {
    // Given: a Counter and a Greeter registered under different names
    let system = ProcessSystem::new().await;
    let counter_name = ProcessName::new("counter");
    let greeter_name = ProcessName::new("greeter");
    let counter_proxy = system
        .spawn(Props::new(|| Counter::new(10)).with_name(counter_name.clone()))
        .await;
    let greeter_proxy = system
        .spawn(Props::new(|| Greeter).with_name(greeter_name.clone()))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: each is looked up by name and communicated with independently
    let counter_any = system
        .lookup_by_name(&counter_name)
        .await
        .expect("counter should be registered");
    let counter = counter_any
        .downcast::<Counter>()
        .expect("downcast to Counter should succeed");
    counter.tell(Increment).await.expect("tell should succeed");
    let count = counter.ask(GetCount).await.expect("ask should succeed");

    let greeter_any = system
        .lookup_by_name(&greeter_name)
        .await
        .expect("greeter should be registered");
    let greeter = greeter_any
        .downcast::<Greeter>()
        .expect("downcast to Greeter should succeed");
    let greeting = greeter
        .ask(Greet("Registry".to_string()))
        .await
        .expect("ask should succeed");

    // Then: each process operates independently — Counter at 11, Greeter returns greeting
    assert_eq!(count, 11); // 10 initial + 1 increment
    assert_eq!(greeting, "Hello, Registry!");

    counter_proxy.stop().await.ok();
    greeter_proxy.stop().await.ok();
}
