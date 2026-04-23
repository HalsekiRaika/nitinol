//! Dead Letter Queue example: capturing undeliverable messages via the built-in DLQ stream.
//!
//! Run with:
//!   cargo run -p dead-letters
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p dead-letters
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).
//!
//! # Learning objectives
//!
//! 1. **Built-in Dead Letter infrastructure**
//!    `ProcessSystem` automatically spawns a `Stream<BoxedMessage>` for the
//!    `$dead-letters` topic at startup. No additional configuration is needed
//!    to enable the DLQ.
//!
//! 2. **Automatic routing on Tell/Ask failure**
//!    When a `tell` or `ask` to a stopped process fails, the runtime wraps the
//!    message in a `DeadLetter { destination, message, sender }` and
//!    automatically publishes it to the DLQ stream.
//!
//! 3. **Inspecting the DeadLetter struct**
//!    A DLQ stream subscriber receives a `BoxedMessage` and can unwrap the
//!    outer layer with `msg.downcast_ref::<DeadLetter>()`, then recover the
//!    inner message with `dl.message.downcast_ref::<Ping>()` (two-stage
//!    downcast).
//!
//! 4. **SuppressDeadLetterLog marker**
//!    Messages whose type implements `SuppressDeadLetterLog` cause the runtime
//!    to suppress only the log output; delivery to the stream continues
//!    unaffected. This confirms that the Pub/Sub mechanism learned in
//!    6.streams is also the foundation of the DLQ.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitinol_runtime::{Props, ProcessSystem};
use tracing::info;

use dead_letters::message::{Hush, Ping, Query};
use dead_letters::observer::{DeadLetterInspector, DeadLetterObserver};
use dead_letters::target::TargetProcess;

// noinspection DuplicatedCode
/// Initialise tracing for this example.
///
/// Without `--features console`:
///   Uses `fmt` + `EnvFilter`.  `RUST_LOG` controls the filter level;
///   defaults to `info` when unset.
///
/// With `--features console` (requires `RUSTFLAGS="--cfg tokio_unstable"`):
///   Adds a `console_subscriber` layer so the process can be inspected with
///   `tokio-console`.  Run with:
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p dead-letters --features console
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

/// Polls `counter` until it reaches at least `expected`, with a 5-second timeout.
async fn wait_for_count(counter: &AtomicU32, expected: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < expected {
        assert!(Instant::now() < deadline, "timed out waiting for dead letter");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::main]
async fn main() {
    init_tracing();

    info!("Dead Letters Example");

    demo_tell_routes_to_dead_letter_stream().await;
    demo_ask_returns_dead_letter_error_and_publishes_to_stream().await;
    demo_suppress_log_marker_still_delivers_to_stream().await;
}

// ─── Demo 1: Tell routes to dead-letter stream ───────────────────────────────
#[tracing::instrument(name = "tell_routes_to_dead_letter_stream")]
async fn demo_tell_routes_to_dead_letter_stream() {
    info!("Tell routes to dead-letter stream");

    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    // Spawn and immediately stop the target so all later messages are undeliverable.
    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    info!("Sending Ping to stopped process (pid={target_pid})...");
    let _ = proxy.tell(Ping).await;

    // Poll until the observer confirms receipt, up to 5 seconds.
    wait_for_count(&received, 1).await;

    info!("DeadLetter received on stream (destination={target_pid}).");
}

// ─── Demo 2: Ask returns AskError::DeadLetter and publishes to stream ────────
#[tracing::instrument(name = "ask_returns_dead_letter_error_and_publishes_to_stream")]
async fn demo_ask_returns_dead_letter_error_and_publishes_to_stream() {
    info!("Ask returns AskError::DeadLetter and publishes to stream");

    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    info!("Asking Query to stopped process (pid={target_pid})...");
    let result = proxy.ask(Query).await;

    match result {
        Err(err) => info!("  ask() returned Err: {err}"),
        Ok(_) => info!("  ask() unexpectedly succeeded"),
    }

    // ask() also routes to the dead-letter stream.
    wait_for_count(&received, 1).await;

    let count = received.load(Ordering::SeqCst);
    info!("  stream also received the dead letter (count={count}).");
}

// ─── Demo 3: SuppressDeadLetterLog still delivers to stream ──────────────────
#[tracing::instrument(name = "suppress_log_marker_still_delivers_to_stream")]
async fn demo_suppress_log_marker_still_delivers_to_stream() {
    info!("SuppressDeadLetterLog still delivers to stream");

    let system = ProcessSystem::new().await;
    let dl_stream = system.dead_letter_stream();

    let received = Arc::new(AtomicU32::new(0));
    let observer_proxy = system
        .spawn(Props::subscriber({
            let received = received.clone();
            move || DeadLetterObserver::new(received.clone())
        }))
        .await;
    dl_stream
        .subscribe(observer_proxy)
        .await
        .expect("subscribe should succeed");

    // Inspector performs the real two-stage downcast and prints actual field values.
    let inspector_proxy = system.spawn(Props::subscriber(|| DeadLetterInspector)).await;
    dl_stream
        .subscribe(inspector_proxy)
        .await
        .expect("subscribe should succeed");

    let proxy = system.spawn(Props::new(|| TargetProcess)).await;
    let target_pid = proxy.pid();
    proxy.stop().await.expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    info!("Sending Hush (SuppressDeadLetterLog) to stopped process (pid={target_pid})...");
    let _ = proxy.tell(Hush).await;

    // Even though Hush suppresses log output, the stream still receives the notification.
    wait_for_count(&received, 1).await;

    info!("  Stream received the dead letter despite SuppressDeadLetterLog.");
    info!("  Log suppression is applied by the runtime — but stream delivery is never suppressed.");
}
