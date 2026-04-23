//! Run with:
//!   cargo run -p messaging
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p messaging
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props};
use tracing::info;

use messaging::counter::Counter;
use messaging::message::{Decrement, GetCount, Increment};

/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p messaging --features console
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

    // Props wraps a factory closure so Counter is created inside the spawned task —
    // ownership never crosses back to this thread.
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;

    let pid = proxy.pid();
    info!("Spawned counter at pid={pid}");

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
    info!("After 3 increments: count={count}");

    proxy.tell(Decrement).await.expect("tell should succeed");

    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    info!("After 1 decrement: count={count}");

    proxy.stop().await.expect("stop should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;
}
