//! Integration tests for the Counter process with Tell/Ask messaging.
//!
//! These tests verify that:
//! - `tell` (fire-and-forget) modifies the Counter state
//! - `ask` (request-response) reads the Counter state
//! - Messages are processed sequentially in send order
//!
//! Because the message channel is FIFO, issuing `ask(GetCount)` after a series
//! of `tell` calls guarantees all tells have been applied before the response
//! arrives — no additional sleep is needed between them.

use std::time::Duration;

use messaging::counter::Counter;
use messaging::message::{Decrement, GetCount, Increment};
use nitinol_runtime::{ProcessSystem, Props};

// ─── Tell: Increment ──────────────────────────────────────────────────────────

#[tokio::test]
async fn tell_increment_increases_count() {
    // Given: a counter starting at 0
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;
    // on_start runs asynchronously; wait before sending messages
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Increment is sent via tell (fire-and-forget)
    proxy.tell(Increment).await.expect("tell should succeed");

    // Then: ask returns count = 1 — the FIFO channel ensures tell is applied first
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 1);

    proxy.stop().await.ok();
}

// ─── Tell: Decrement ──────────────────────────────────────────────────────────

#[tokio::test]
async fn tell_decrement_decreases_count() {
    // Given: a counter starting at 3
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(3))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Decrement is sent via tell
    proxy.tell(Decrement).await.expect("tell should succeed");

    // Then: ask returns count = 2
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 2);

    proxy.stop().await.ok();
}

#[tokio::test]
async fn tell_decrement_saturates_at_zero() {
    // Given: a counter starting at 0
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: Decrement is sent on a counter already at zero
    proxy.tell(Decrement).await.expect("tell should succeed");

    // Then: saturating_sub prevents u32 underflow — count stays at 0
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 0);

    proxy.stop().await.ok();
}

// ─── Ask: GetCount ────────────────────────────────────────────────────────────

#[tokio::test]
async fn ask_get_count_returns_initial_value() {
    // Given: a counter starting at 42 with no mutations applied
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(42))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: GetCount is asked without prior tell messages
    let count = proxy.ask(GetCount).await.expect("ask should succeed");

    // Then: the initial value is returned unchanged
    assert_eq!(count, 42);

    proxy.stop().await.ok();
}

// ─── Sequential ordering ──────────────────────────────────────────────────────

#[tokio::test]
async fn multiple_tells_processed_sequentially() {
    // Given: a counter starting at 0
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: three Increments are queued via tell
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");

    // Then: all three are applied before the ask response arrives — count = 3
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 3);

    proxy.stop().await.ok();
}

// ─── Combined workflow ────────────────────────────────────────────────────────

#[tokio::test]
async fn tell_and_ask_combined_workflow() {
    // Given: a counter starting at 0
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| Counter::new(0))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: three Increments then one Decrement are queued
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Increment).await.expect("tell should succeed");
    proxy.tell(Decrement).await.expect("tell should succeed");

    // Then: count = 2 (3 increments − 1 decrement)
    let count = proxy.ask(GetCount).await.expect("ask should succeed");
    assert_eq!(count, 2);

    proxy.stop().await.ok();
}
