use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props};

use messaging::counter::Counter;
use messaging::message::{Decrement, GetCount, Increment};

#[tokio::main]
async fn main() {
    let system = ProcessSystem::new().await;

    // Props wraps a factory closure so Counter is created inside the spawned task —
    // ownership never crosses back to this thread.
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;

    println!("Spawned counter at pid={}", proxy.pid());

    // Wait for on_start to complete before sending messages.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Tell = fire-and-forget (write).
    // The proxy is the only handle to the Counter living inside the tokio task.
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");

    // Ask = request-response (read).
    // Because the channel is FIFO, issuing ask after tell guarantees all
    // tells have been applied before the response arrives.
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    println!("After 3 increments: count={count}");

    proxy.tell(Decrement).await.expect("tell should succeed");

    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    println!("After 1 decrement: count={count}");

    proxy.stop().await.expect("stop should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;
}
