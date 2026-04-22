//! Supervision example: demonstrates Stop and Restart supervision strategies.
//!
//! Run with:
//!   cargo run -p supervision
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

use supervision::transformer::{DataTransformer, GetSuccessCount, Parse};

#[tokio::main]
async fn main() {
    println!("=== Supervision Example ===\n");

    demo_stop_strategy().await;
    demo_restart_strategy().await;
    demo_rate_limit_exceeded().await;
}

// ─── Stop strategy ───────────────────────────────────────────────────────────

async fn demo_stop_strategy() {
    println!("--- Stop strategy (default) ---");
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let ok = proxy.ask(Parse("21".to_string())).await.unwrap();
    println!("Parse(\"21\") → {ok}");

    let err = proxy.ask(Parse("oops".to_string())).await;
    println!("Parse(\"oops\") → {err:?}  (handler returned ParseError)");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let found = system.lookup(proxy.pid()).await;
    let status = if found.is_some() { "Some(..)" } else { "None" };
    println!("lookup after error → {status}  (None = process stopped)\n");
}

// ─── Restart strategy ────────────────────────────────────────────────────────

async fn demo_restart_strategy() {
    println!("--- Restart strategy (max_retries=2, within=10s) ---");
    let system = ProcessSystem::new().await;
    let mut props = Props::new(DataTransformer::new);
    props.with_supervision_strategy(SupervisionStrategy::Restart {
        max_retries: 2,
        within: Duration::from_secs(10),
    });
    let proxy = system.spawn(props).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    proxy.ask(Parse("5".to_string())).await.unwrap();
    proxy.ask(Parse("10".to_string())).await.unwrap();
    let count = proxy.ask(GetSuccessCount).await.unwrap();
    println!("success_count before error = {count}");

    // Trigger a restart — factory closure re-invoked, success_count resets.
    let _ = proxy.ask(Parse("bad".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let count_after = proxy.ask(GetSuccessCount).await.unwrap();
    println!("success_count after restart = {count_after}  (fresh instance → 0)");

    let ok = proxy.ask(Parse("7".to_string())).await.unwrap();
    println!("Parse(\"7\") after restart → {ok}");

    proxy.stop().await.ok();
    println!();
}

// ─── Rate limit exceeded ──────────────────────────────────────────────────────

async fn demo_rate_limit_exceeded() {
    println!("--- Rate limit exceeded (max_retries=2, within=10s) ---");
    let system = ProcessSystem::new().await;
    let mut props = Props::new(DataTransformer::new);
    props.with_supervision_strategy(SupervisionStrategy::Restart {
        max_retries: 2,
        within: Duration::from_secs(10),
    });
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
    println!("lookup after 3 failures → {status}  (None = permanently stopped)\n");
}
