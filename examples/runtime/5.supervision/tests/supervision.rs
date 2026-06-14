//! Integration tests for the supervision example.
//!
//! These tests verify:
//! - Normal input is parsed and doubled correctly ("42" → "42 * 2 = 84")
//! - success_count increments on each successful parse
//! - Invalid input causes the handler to return ParseError (surfaced as AskError::Handler)
//! - Stop strategy: a handler error causes the process to stop and unregister
//! - Restart strategy: a handler error causes the process to restart (stays in registry)
//! - Restart resets success_count to 0 — the factory closure creates a brand-new instance
//! - The restarted instance handles subsequent valid messages correctly
//! - Rate limit exceeded causes permanent stop and unregister

use std::time::Duration;

use nitinol_runtime::{ProcessSystem, Props, SupervisionStrategy};

use supervision::transformer::{DataTransformer, GetSuccessCount, Parse};

// ─── valid_input_returns_doubled_result ──────────────────────────────────────

#[tokio::test]
async fn valid_input_returns_doubled_result() {
    // Given: a DataTransformer with the default Stop strategy
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a valid numeric string is sent
    let result = proxy
        .ask(Parse("42".to_string()))
        .await
        .expect("ask should succeed for valid input");

    // Then: the response is the formatted doubled value
    assert_eq!(result, "42 * 2 = 84");

    proxy.stop().await.ok();
}

// ─── valid_input_of_zero_returns_zero ────────────────────────────────────────

#[tokio::test]
async fn valid_input_of_zero_returns_zero() {
    // Given: a DataTransformer with the default Stop strategy
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: "0" is sent (boundary: smallest non-negative integer)
    let result = proxy
        .ask(Parse("0".to_string()))
        .await
        .expect("ask should succeed for zero");

    // Then: "0 * 2 = 0"
    assert_eq!(result, "0 * 2 = 0");

    proxy.stop().await.ok();
}

// ─── success_count_increments_on_each_valid_input ────────────────────────────

#[tokio::test]
async fn success_count_increments_on_each_valid_input() {
    // Given: a DataTransformer with the default Stop strategy
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: three valid messages are sent sequentially
    proxy
        .ask(Parse("1".to_string()))
        .await
        .expect("ask should succeed");
    proxy
        .ask(Parse("2".to_string()))
        .await
        .expect("ask should succeed");
    proxy
        .ask(Parse("3".to_string()))
        .await
        .expect("ask should succeed");

    // Then: GetSuccessCount reflects all three successful parses
    let count = proxy
        .ask(GetSuccessCount)
        .await
        .expect("ask GetSuccessCount should succeed");
    assert_eq!(count, 3);

    proxy.stop().await.ok();
}

// ─── invalid_input_returns_parse_error ───────────────────────────────────────

#[tokio::test]
async fn invalid_input_returns_parse_error() {
    // Given: a DataTransformer with the default Stop strategy
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: a non-numeric string is sent
    let result = proxy.ask(Parse("not_a_number".to_string())).await;

    // Then: the ask returns Err(AskError::Handler(ParseError { .. }))
    // The handler returned Err, so the caller sees the error via AskError::Handler
    assert!(result.is_err(), "non-numeric input should return an error");

    proxy.stop().await.ok();
}

// ─── stop_strategy_unregisters_process_after_error ───────────────────────────

#[tokio::test]
async fn stop_strategy_unregisters_process_after_error() {
    // Given: a DataTransformer with the default Stop strategy
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(DataTransformer::new)).await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an invalid message triggers ParseError; supervision Stop fires after the reply
    let _ = proxy.ask(Parse("abc".to_string())).await;
    // Allow the supervision stop and registry unregister to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: the process is unregistered from the registry
    let found = system.lookup(pid).await;
    assert!(
        found.is_none(),
        "stopped process should be unregistered from the registry"
    );
}

// ─── restart_strategy_keeps_process_in_registry ──────────────────────────────

#[tokio::test]
async fn restart_strategy_keeps_process_in_registry() {
    // Given: a DataTransformer with Restart{max_retries: 3, within: 10s}
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // When: an invalid message triggers ParseError; supervision Restart fires
    let _ = proxy.ask(Parse("bad_input".to_string())).await;
    // Allow the restart (on_stop + factory + on_start) to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: the process is still registered — it restarted, not stopped
    let found = system.lookup(pid).await;
    assert!(
        found.is_some(),
        "restarted process should remain in the registry"
    );

    proxy.stop().await.ok();
}

// ─── restart_resets_success_count_to_zero ────────────────────────────────────

#[tokio::test]
async fn restart_resets_success_count_to_zero() {
    // Given: a DataTransformer that has accumulated successes before an error
    //
    // This test demonstrates WHY Props uses a factory closure:
    // on restart the runtime calls the closure again, creating a fresh instance
    // whose success_count starts at 0 — internal state is not preserved.
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));
    let proxy = system.spawn(props).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Accumulate 2 successful parses on the original instance
    proxy
        .ask(Parse("10".to_string()))
        .await
        .expect("ask should succeed");
    proxy
        .ask(Parse("20".to_string()))
        .await
        .expect("ask should succeed");
    let count_before = proxy
        .ask(GetSuccessCount)
        .await
        .expect("ask GetSuccessCount should succeed");
    assert_eq!(count_before, 2, "precondition: 2 successes before restart");

    // When: an invalid message triggers a restart (factory closure re-invoked)
    let _ = proxy.ask(Parse("not_a_number".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: success_count is 0 on the new instance
    let count_after = proxy
        .ask(GetSuccessCount)
        .await
        .expect("ask GetSuccessCount should succeed after restart");
    assert_eq!(
        count_after, 0,
        "the factory closure must produce a fresh instance with success_count=0"
    );

    proxy.stop().await.ok();
}

// ─── restarted_process_handles_subsequent_valid_messages ─────────────────────

#[tokio::test]
async fn restarted_process_handles_subsequent_valid_messages() {
    // Given: a DataTransformer that has already restarted once after a parse error
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new)
        .with_supervision_strategy(SupervisionStrategy::restart(3, Duration::from_secs(10)).expect("valid restart config"));
    let proxy = system.spawn(props).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger a restart
    let _ = proxy.ask(Parse("oops".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // When: a valid message is sent to the restarted instance
    let result = proxy.ask(Parse("7".to_string())).await;

    // Then: the restarted instance processes the message correctly
    assert!(
        result.is_ok(),
        "restarted process should handle valid messages"
    );
    assert_eq!(result.unwrap(), "7 * 2 = 14");

    proxy.stop().await.ok();
}

// ─── rate_limit_exceeded_causes_permanent_stop ───────────────────────────────

#[tokio::test]
async fn rate_limit_exceeded_causes_permanent_stop() {
    // Given: a DataTransformer with Restart{max_retries: 2, within: 10s}
    //
    // Failure sequence within the 10-second window:
    //   Fail #1 → restart (retry_count=1, 1 ≤ 2 → allowed)
    //   Fail #2 → restart (retry_count=2, 2 ≤ 2 → allowed)
    //   Fail #3 → stop    (retry_count=3, 3 > 2 → denied → permanent stop)
    let system = ProcessSystem::new().await;
    let props = Props::new(DataTransformer::new)
        .with_supervision_strategy(SupervisionStrategy::restart(2, Duration::from_secs(10)).expect("valid restart config"));
    let proxy = system.spawn(props).await;
    let pid = proxy.pid();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger 2 restarts (within the allowed limit)
    let _ = proxy.ask(Parse("bad1".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = proxy.ask(Parse("bad2".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Trigger the third failure — exceeds the rate limit → permanent stop
    let _ = proxy.ask(Parse("bad3".to_string())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then: the process is permanently stopped and unregistered
    let found = system.lookup(pid).await;
    assert!(
        found.is_none(),
        "process should be unregistered after the restart rate limit is exceeded"
    );
}
