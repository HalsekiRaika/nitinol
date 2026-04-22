use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{ProcessSystem, Props};

use registry::counter::{Counter, Decrement, GetCount, Increment};
use registry::greeter::{Greet, Greeter};

#[tokio::main]
async fn main() {
    let system = ProcessSystem::new().await;

    // ── Named spawning ────────────────────────────────────────────────────────
    //
    // `spawn_named` registers the process under a stable `ProcessName`.
    // Unlike an anonymous `spawn`, this makes the process discoverable by
    // name from anywhere that holds a reference to the system.
    let counter_name = ProcessName::new("counter");
    let greeter_name = ProcessName::new("greeter");

    let counter_proxy = system
        .spawn_named(counter_name.clone(), Props::new(|| Counter::new(0)))
        .await;
    let greeter_proxy = system
        .spawn_named(greeter_name.clone(), Props::new(|| Greeter))
        .await;

    println!("Spawned counter at pid={}", counter_proxy.pid());
    println!("Spawned greeter at pid={}", greeter_proxy.pid());

    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Discovery by name ─────────────────────────────────────────────────────
    //
    // `lookup_by_name` searches the process registry and returns an `AnyProxy`.
    // A `ProcessName` survives restarts and is the preferred stable identifier.
    // Use it when the proxy may have been discarded but the name is known.
    let counter_any = system
        .lookup_by_name(&counter_name)
        .await
        .expect("counter should be registered");
    println!("Found counter by name");

    // ── Type recovery via downcast ────────────────────────────────────────────
    //
    // `AnyProxy` is type-erased. Downcast recovers the typed `ProcessProxy<P>`
    // so that type-safe tell/ask calls can be made.
    // If the concrete type does not match, `downcast` returns `None`.
    let counter = counter_any
        .downcast::<Counter>()
        .expect("downcast to Counter should succeed");

    counter.tell(Increment).await.expect("tell should succeed");
    counter.tell(Increment).await.expect("tell should succeed");
    counter.tell(Decrement).await.expect("tell should succeed");
    let count = counter.ask(GetCount).await.expect("ask should succeed");
    println!("Counter value after 2 increments and 1 decrement: {count}");

    // ── Discovery by Pid ──────────────────────────────────────────────────────
    //
    // `lookup(Pid)` also returns an `AnyProxy`.
    // Pid is Copy-able and convenient within a session, but it is not stable
    // across restarts — use `ProcessName` for persistent references.
    let greeter_pid = greeter_proxy.pid();
    let greeter_any = system
        .lookup(greeter_pid)
        .await
        .expect("greeter should be registered");
    println!("Found greeter by pid={greeter_pid}");

    let greeter = greeter_any
        .downcast::<Greeter>()
        .expect("downcast to Greeter should succeed");
    let greeting = greeter
        .ask(Greet("Registry".to_string()))
        .await
        .expect("ask should succeed");
    println!("Greeter says: {greeting}");

    // ── Cleanup ───────────────────────────────────────────────────────────────
    counter_proxy.stop().await.expect("stop should succeed");
    greeter_proxy.stop().await.expect("stop should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;
}
