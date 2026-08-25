//! Supervision example: demonstrates Stop and Restart supervision strategies.
//!
//! Run with:
//!   cargo run -p supervision
//!
//! To enable tokio-console (opt-in):
//!   set RUSTFLAGS="--cfg tokio_unstable"
//!   cargo run --features console -p supervision
//!
//! RUST_LOG can be set to override the default log level (defaults to `info`).
//!
//! # Learning objectives
//!
//! 1. **Why Props?**
//!    `Props::new(DataTransformer::new)` stores a *factory closure*, not an
//!    instance.  On restart the runtime calls the closure again, producing a
//!    fresh `DataTransformer` with `success_count = 0`.  Moving an instance
//!    directly into a task would make restart impossible.
//!
//! 2. **Stop strategy** (default)
//!    One handler error → process is stopped and unregistered.
//!    Use when partial failure is unacceptable.
//!
//! 3. **Restart strategy**
//!    Handler errors within the sliding time window are retried up to
//!    `max_retries` times.  Exceeding the limit causes a permanent stop.
//!    Use for transient errors that a fresh instance can recover from.
//!
//! # Future note
//! After CQRS+ES integration, replaying events from the event store will allow
//! a restarted process to reconstruct pre-crash state — Restart then preserves
//! domain history across failures.

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props, SupervisionStrategy};
use tracing::info;

use supervision::transformer::{DataTransformer, GetSuccessCount, Parse};

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
///     RUSTFLAGS="--cfg tokio_unstable" cargo run -p supervision --features console
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

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

    info!("Supervision Example");

    demo_stop_strategy().await;
    demo_restart_strategy().await;
    demo_rate_limit_exceeded().await;
}

// ─── Stop strategy ───────────────────────────────────────────────────────────

#[tracing::instrument(name = "demo-stop-strategy")]
async fn demo_stop_strategy() {
    info!("Stop strategy (default)");
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let ok = proxy
        .ask(Parse("21".to_string()))
        .await
        .expect("\"21\" parses, and the process is still alive on its first message");
    info!("Parse(\"21\") → {ok}");

    let err = proxy.ask(Parse("oops".to_string())).await;
    info!("Parse(\"oops\") → {err:?}  (handler returned ParseError)");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let found = system.lookup(proxy.pid()).await;
    let status = if found.is_some() { "Some(..)" } else { "None" };
    info!("lookup after error → {status}  (None = process stopped)");
}

// ─── Restart strategy ────────────────────────────────────────────────────────

#[tracing::instrument(name = "demo-restart-strategy")]
async fn demo_restart_strategy() {
    info!("Restart strategy (max_retries=2, within=10s)");
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new).with_supervision_strategy(
        SupervisionStrategy::restart(2, Duration::from_secs(10)).expect("valid restart config"),
    );
    let proxy = system.spawn(props).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    proxy
        .ask(Parse("5".to_string()))
        .await
        .expect("\"5\" parses, so the process stays alive and answers");
    proxy
        .ask(Parse("10".to_string()))
        .await
        .expect("\"10\" parses, so the process stays alive and answers");
    let count = proxy
        .ask(GetSuccessCount)
        .await
        .expect("GetSuccessCount never fails and the process has not been stopped yet");
    info!("success_count before error = {count}");

    // Trigger a restart — factory closure re-invoked, success_count resets.
    let _ = proxy.ask(Parse("bad".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let count_after = proxy.ask(GetSuccessCount).await.expect(
        "the first failure is within max_retries, so the process was restarted, not stopped",
    );
    info!("success_count after restart = {count_after}  (fresh instance → 0)");

    let ok = proxy
        .ask(Parse("7".to_string()))
        .await
        .expect("\"7\" parses, and the restarted instance accepts new messages");
    info!("Parse(\"7\") after restart → {ok}");

    proxy.stop().await.ok();
}

// ─── Rate limit exceeded ──────────────────────────────────────────────────────

#[tracing::instrument(name = "demo-rate-limit-exceeded")]
async fn demo_rate_limit_exceeded() {
    info!("Rate limit exceeded (max_retries=2, within=10s)");
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new).with_supervision_strategy(
        SupervisionStrategy::restart(2, Duration::from_secs(10)).expect("valid restart config"),
    );
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 3 consecutive failures: restarts #1 and #2 are allowed, #3 exceeds max_retries.
    for i in 1..=3 {
        let _ = proxy.ask(Parse(format!("bad{i}"))).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let found = system.lookup(pid).await;
    let status = if found.is_some() { "Some(..)" } else { "None" };
    info!("lookup after 3 failures → {status}  (None = permanently stopped)");
}
