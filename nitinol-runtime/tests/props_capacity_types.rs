//! Tests for the `bounded(n)` smart constructors on the capacity enums.
//!
//! Pinned-down contract: `MailboxCapacity`, `StashCapacity`, and `PipeCapacity`
//! each have a `bounded(n: usize) -> Result<Self, ConfigError>` constructor that
//! - returns `Err(ConfigError::ZeroCapacity)` for `n == 0`, so no caller can
//!   produce a zero-sized buffer via the public path; and
//! - returns `Ok(Bounded(n))` for `n > 0`, round-tripping the value into the
//!   typed variant.

use nitinol_runtime::error::ConfigError;
use nitinol_runtime::{MailboxCapacity, PipeCapacity, StashCapacity};

// MailboxCapacity

/// Given the `bounded(0)` smart constructor,
/// when invoked with zero,
/// then it returns `Err(ConfigError::ZeroCapacity)` — `Bounded(NonZeroUsize)`
/// is the only public route, and `0` cannot escape into the channel layer.
#[test]
fn mailbox_capacity_bounded_zero_returns_zero_capacity_error() {
    let result = MailboxCapacity::bounded(0);
    assert!(matches!(result, Err(ConfigError::ZeroCapacity)));
}

/// Given the `bounded(n)` smart constructor with `n > 0`,
/// when invoked,
/// then it returns `Ok(Bounded(NonZeroUsize::new(n).unwrap()))`.
#[test]
fn mailbox_capacity_bounded_positive_returns_ok_bounded() {
    let result = MailboxCapacity::bounded(7).expect("bounded(7) must succeed");
    match result {
        MailboxCapacity::Bounded(n) => assert_eq!(n.get(), 7),
        _ => panic!("expected MailboxCapacity::Bounded"),
    }
}

// StashCapacity (same shape — separate tests so a regression points at the
// affected enum directly, not at a single shared parameterized test).

#[test]
fn stash_capacity_bounded_zero_returns_zero_capacity_error() {
    let result = StashCapacity::bounded(0);
    assert!(matches!(result, Err(ConfigError::ZeroCapacity)));
}

#[test]
fn stash_capacity_bounded_positive_returns_ok_bounded() {
    let result = StashCapacity::bounded(16).expect("bounded(16) must succeed");
    match result {
        StashCapacity::Bounded(n) => assert_eq!(n.get(), 16),
        _ => panic!("expected StashCapacity::Bounded"),
    }
}

// PipeCapacity

#[test]
fn pipe_capacity_bounded_zero_returns_zero_capacity_error() {
    let result = PipeCapacity::bounded(0);
    assert!(matches!(result, Err(ConfigError::ZeroCapacity)));
}

#[test]
fn pipe_capacity_bounded_positive_returns_ok_bounded() {
    let result = PipeCapacity::bounded(4).expect("bounded(4) must succeed");
    match result {
        PipeCapacity::Bounded(n) => assert_eq!(n.get(), 4),
        _ => panic!("expected PipeCapacity::Bounded"),
    }
}
