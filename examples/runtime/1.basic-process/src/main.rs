use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props};

use basic_process::counter::Counter;

#[tokio::main]
async fn main() {
    // ProcessSystem initialises the runtime and the built-in dead-letter actor.
    let system = ProcessSystem::new().await;

    // Props wraps a factory closure, not a Counter instance directly.
    // The closure runs inside the spawned tokio task, so the Counter is
    // created there — ownership never crosses back to this thread.
    // The same factory can be invoked again on restart, which is why a
    // closure is required rather than a pre-built value.
    let props = Props::new(|| Counter::new(0));

    // After `spawn`, the Counter lives inside a tokio task.
    // Only `proxy` remains on this side of the ownership boundary.
    let proxy = system.spawn(props).await;

    println!("Spawned counter at pid={}", proxy.pid());

    // `on_start` runs asynchronously inside the task.
    // A brief wait ensures the lifecycle hook completes before we stop.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stop is delivered as a channel signal through the proxy.
    // There is no way to call `drop(counter)` directly from here —
    // the proxy is the only handle the caller holds.
    proxy.stop().await.expect("stop should succeed");

    // Allow the lifecycle task to finish `on_stop` before the process exits.
    tokio::time::sleep(Duration::from_millis(100)).await;
}
