//! Run with:
//!   cargo run -p registry
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p registry
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).

use std::time::Duration;

use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{ProcessSystem, Props};
use tracing::info;

use registry::counter::{Counter, Decrement, GetCount, Increment};
use registry::greeter::{Greet, Greeter};

/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p registry --features console
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    #[cfg(not(feature = "console"))]
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    #[cfg(feature = "console")]
    tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

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

    let counter_pid = counter_proxy.pid();
    let greeter_pid = greeter_proxy.pid();
    info!("Spawned counter at pid={counter_pid}");
    info!("Spawned greeter at pid={greeter_pid}");

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
    info!("Found counter by name");

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
    info!("Counter value after 2 increments and 1 decrement: {count}");

    // ── Discovery by Pid ──────────────────────────────────────────────────────
    //
    // `lookup(Pid)` also returns an `AnyProxy`.
    // Pid is Copy-able and convenient within a session, but it is not stable
    // across restarts — use `ProcessName` for persistent references.
    let greeter_any = system
        .lookup(greeter_pid)
        .await
        .expect("greeter should be registered");
    info!("Found greeter by pid={greeter_pid}");

    let greeter = greeter_any
        .downcast::<Greeter>()
        .expect("downcast to Greeter should succeed");
    let greeting = greeter
        .ask(Greet("Registry".to_string()))
        .await
        .expect("ask should succeed");
    info!("Greeter says: {greeting}");

    // ── Cleanup ───────────────────────────────────────────────────────────────
    counter_proxy.stop().await.expect("stop should succeed");
    greeter_proxy.stop().await.expect("stop should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;
}
