//! Tests for the `SupervisionStrategy` smart constructors.
//!
//! The earlier code validated `Restart { within }` lazily via `assert!` inside
//! `Props::into_parts()`, so a zero-duration window was only caught at spawn
//! time and propagated as a panic. Three smart constructors on
//! `SupervisionStrategy` replace that:
//!
//! ```text
//! impl SupervisionStrategy {
//!     pub fn stop() -> Self;
//!     pub fn resume() -> Self;
//!     pub fn restart(max_retries: u32, within: Duration) -> Result<Self, ConfigError>;
//! }
//! ```
//!
//! These tests pin down:
//! - `stop()` returns the `Stop` variant.
//! - `resume()` returns the `Resume` variant.
//! - `restart(_, Duration::ZERO)` returns `Err(ConfigError::InvalidRestartWithin)`
//!   so that the rate-limit window is never silently dropped (the bug class the
//!   former `assert!` was meant to catch, lifted into the type system).
//! - `restart(n, within > 0)` returns `Ok(Restart { max_retries: n, within })`
//!   with the supplied fields intact (round-trip).
//! - `max_retries = 0` is accepted (it is a legitimate policy: "no restart, but
//!   surface the rate-limit shape"); only the `Duration::ZERO` check is the
//!   error path.

use std::time::Duration;

use nitinol_runtime::error::ConfigError;
use nitinol_runtime::SupervisionStrategy;

// stop() / resume()

/// `SupervisionStrategy::stop()` returns the `Stop` variant — the canonical
/// "no restart, on_stop runs" policy.
#[test]
fn supervision_strategy_stop_returns_stop_variant() {
    let s = SupervisionStrategy::stop();
    assert!(matches!(s, SupervisionStrategy::Stop));
}

/// `SupervisionStrategy::resume()` returns the `Resume` variant — the
/// "swallow handler error, keep running" policy.
#[test]
fn supervision_strategy_resume_returns_resume_variant() {
    let s = SupervisionStrategy::resume();
    assert!(matches!(s, SupervisionStrategy::Resume));
}

// restart(_, Duration::ZERO) is the failure case

/// Given `Duration::ZERO` as the rate-limit window,
/// when `SupervisionStrategy::restart(3, Duration::ZERO)` is called,
/// then it returns `Err(ConfigError::InvalidRestartWithin)`.
///
/// This is the key contract change: the earlier code accepted the value at
/// construction time and panicked at `Props::into_parts()` via
/// `assert!`. The new path rejects it eagerly at the public boundary so no
/// invalid policy can reach the spawn path.
#[test]
fn supervision_strategy_restart_with_zero_within_returns_invalid_restart_within() {
    let result = SupervisionStrategy::restart(3, Duration::ZERO);
    assert!(
        matches!(result, Err(ConfigError::InvalidRestartWithin)),
        "restart(_, Duration::ZERO) must return Err(ConfigError::InvalidRestartWithin); \
         got {result:?}"
    );
}

/// Even with `max_retries == 0`, a zero-duration window must still fail —
/// the rejection is on the `within` field, not on a combination.
#[test]
fn supervision_strategy_restart_zero_retries_with_zero_within_still_fails() {
    let result = SupervisionStrategy::restart(0, Duration::ZERO);
    assert!(matches!(result, Err(ConfigError::InvalidRestartWithin)));
}

// restart(n, within > 0) round-trip

/// Given `max_retries = 3` and `within = 60s`,
/// when `SupervisionStrategy::restart(...)` is called,
/// then it returns `Ok(Restart { max_retries: 3, within: 60s })`.
///
/// Round-trip: the smart constructor must not mutate the supplied fields.
#[test]
fn supervision_strategy_restart_round_trips_fields() {
    let within = Duration::from_secs(60);
    let s = SupervisionStrategy::restart(3, within).expect("restart(3, 60s) must succeed");
    match s {
        SupervisionStrategy::Restart(config) => {
            assert_eq!(config.max_retries(), 3);
            assert_eq!(config.within(), within);
        }
        other => panic!("expected Restart variant, got {other:?}"),
    }
}

/// `max_retries == 0` is legitimate: it means "no restart attempts allowed —
/// the first failure exits". Only `Duration::ZERO` triggers the error path.
#[test]
fn supervision_strategy_restart_accepts_zero_max_retries_when_within_is_positive() {
    let s = SupervisionStrategy::restart(0, Duration::from_millis(1))
        .expect("restart(0, 1ms) must succeed: only within=ZERO is rejected");
    assert!(matches!(&s, SupervisionStrategy::Restart(c) if c.max_retries() == 0));
}

/// A 1-nanosecond `within` is positive: it must be accepted. The boundary is
/// at `Duration::ZERO`, not "obviously useful".
#[test]
fn supervision_strategy_restart_accepts_smallest_positive_duration() {
    let result = SupervisionStrategy::restart(1, Duration::from_nanos(1));
    assert!(
        result.is_ok(),
        "1ns is > 0 and must be accepted; got {result:?}"
    );
}

// Trait-level: ConfigError flows like a normal error type so the smart
// constructor can compose with `?` in `Props` builder chains.

/// The `?` operator must work on `restart`'s `Result<_, ConfigError>` inside
/// a function returning `Result<_, ConfigError>`. Compile-time check; if
/// `ConfigError` ever loses its `From<Self>` (identity) or `std::error::Error`
/// impl, this stops compiling.
#[allow(dead_code)]
fn _assert_restart_result_is_question_mark_friendly() -> Result<SupervisionStrategy, ConfigError> {
    let s = SupervisionStrategy::restart(3, Duration::from_secs(1))?;
    Ok(s)
}
