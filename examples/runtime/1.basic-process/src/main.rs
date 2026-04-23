//! Run with:
//!   cargo run -p basic-process
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p basic-process
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props};
use tracing::info;

use basic_process::counter::Counter;

/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p basic-process --features console
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

    let pid = proxy.pid();
    info!("Spawned counter at pid={pid}");

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
