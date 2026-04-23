//! Run with:
//!   cargo run -p watching
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p watching
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props};
use tracing::info;

use watching::collector::{Collect, Collector, GetCollected};
use watching::producer::{Producer, Pull};
use watching::transformer::{Transform, Transformer};

/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p watching --features console
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

    // ── Pipeline construction (bottom-up) ─────────────────────────────────────
    //
    // Build the pipeline from tail to head so each stage can receive the pid
    // of its downstream before it is spawned.  The watch relationships are
    // registered inside each process's `on_start` hook.
    //
    //   Producer  →watches→  Transformer  →watches→  Collector
    //
    let collector_proxy = system.spawn(Props::new(Collector::default)).await;
    let collector_pid = collector_proxy.pid();

    let transformer_proxy = system
        .spawn(Props::new(move || Transformer::new(collector_pid)))
        .await;
    let transformer_pid = transformer_proxy.pid();

    // `items` is cloned inside the closure because `Fn()` may be called more
    // than once (e.g., if a supervision restart were configured).
    let items = vec!["hello".to_string(), "world".to_string(), "watching".to_string()];
    let producer_proxy = system
        .spawn(Props::new(move || Producer::new(items.clone(), transformer_pid)))
        .await;

    // Allow on_start hooks and watch registrations to complete before use.
    tokio::time::sleep(Duration::from_millis(100)).await;

    info!("Normal data flow");

    // ── Normal data flow: Pull → Transform → Collect ──────────────────────────
    //
    // The main function drives the pipeline manually, which keeps each stage's
    // responsibility clear: Producer vends, Transformer converts, Collector stores.
    // The explicit three-step loop makes the data path visible for learning.
    for _ in 0..3 {
        let item = producer_proxy
            .ask(Pull)
            .await
            .expect("ask Pull should succeed")
            .expect("item should be present");

        let transformed = transformer_proxy
            .ask(Transform(item))
            .await
            .expect("ask Transform should succeed");

        collector_proxy
            .tell(Collect(transformed))
            .await
            .expect("tell Collect should succeed");
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    let collected = collector_proxy
        .ask(GetCollected)
        .await
        .expect("ask GetCollected should succeed");
    info!("Collected items: {collected:?}");

    info!("Cascade-stop demonstration");

    // ── Cascade-stop: Collector stops → Transformer errors → Producer notified ─
    //
    // 1. Collector is stopped normally.
    // 2. Transformer receives `Terminated { why: Stopped }` via `on_terminated`
    //    and sets `downstream_alive = false`.
    // 3. The next `Transform` message causes the handler to return
    //    `Err(DownstreamTerminated)`.  `SupervisionStrategy::Stop` fires and
    //    stops the Transformer.
    // 4. Producer receives `Terminated { why: Stopped }` for the Transformer.
    //
    // This shows how a single downstream failure propagates up through the
    // pipeline using only the watch API — no explicit coordination needed.
    collector_proxy.stop().await.expect("stop Collector should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger the error path in Transformer's handler.
    transformer_proxy
        .tell(Transform("trigger".to_string()))
        .await
        .ok(); // process may have stopped; ignore SendError
    tokio::time::sleep(Duration::from_millis(100)).await;

    info!("Pipeline cascade-stop complete.");

    producer_proxy.stop().await.ok();
    tokio::time::sleep(Duration::from_millis(50)).await;
}
