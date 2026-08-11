//! Tests for the capacity enums and the ConfigError type that backs
//! `bounded(0)` rejection.
//!
//! Pinned-down contract:
//! - `MailboxCapacity`, `StashCapacity`, and `PipeCapacity` each expose
//!   `Inherit` and `Bounded(NonZeroUsize)` variants — one uniform shape
//!   shared by all three capacity enums.
//! - Each capacity type has a `bounded(n: usize) -> Result<Self, ConfigError>`
//!   smart constructor that:
//!     - returns `Err(ConfigError::ZeroCapacity)` for `n == 0`, so no caller
//!       can produce a zero-sized buffer via the public path; and
//!     - returns `Ok(Bounded(NonZeroUsize::new(n).unwrap()))` for `n > 0`,
//!       round-tripping the value into the typed variant.
//! - `ConfigError` exposes the two variants used in this issue
//!   (`ZeroCapacity`, `InvalidRestartWithin`) so smart constructors elsewhere
//!   can share a single error type.

use std::num::NonZeroUsize;

use nitinol_runtime::error::ConfigError;
use nitinol_runtime::{MailboxCapacity, PipeCapacity, StashCapacity};

// MailboxCapacity

/// Given the `Inherit` variant of `MailboxCapacity`,
/// when constructed directly,
/// then the value matches the `Inherit` pattern.
///
/// This pins down that `Inherit` is constructible without any argument and is
/// the "read the System default" marker the spawn boundary expects.
#[test]
fn mailbox_capacity_inherit_variant_exists() {
    let cap = MailboxCapacity::Inherit;
    assert!(matches!(cap, MailboxCapacity::Inherit));
}

/// Given a `NonZeroUsize` of 32,
/// when wrapped in `MailboxCapacity::Bounded`,
/// then the inner value is preserved.
///
/// Pins down that `Bounded` carries a `NonZeroUsize` payload — the type that
/// the spawn boundary then forwards as the concrete mailbox channel size.
#[test]
fn mailbox_capacity_bounded_carries_nonzero_usize() {
    let n = NonZeroUsize::new(32).expect("32 is non-zero");
    let cap = MailboxCapacity::Bounded(n);
    match cap {
        MailboxCapacity::Bounded(inner) => assert_eq!(inner, n),
        _ => panic!("expected MailboxCapacity::Bounded"),
    }
}

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
fn stash_capacity_inherit_variant_exists() {
    let cap = StashCapacity::Inherit;
    assert!(matches!(cap, StashCapacity::Inherit));
}

#[test]
fn stash_capacity_bounded_carries_nonzero_usize() {
    let n = NonZeroUsize::new(64).expect("64 is non-zero");
    let cap = StashCapacity::Bounded(n);
    match cap {
        StashCapacity::Bounded(inner) => assert_eq!(inner, n),
        _ => panic!("expected StashCapacity::Bounded"),
    }
}

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
fn pipe_capacity_inherit_variant_exists() {
    let cap = PipeCapacity::Inherit;
    assert!(matches!(cap, PipeCapacity::Inherit));
}

#[test]
fn pipe_capacity_bounded_carries_nonzero_usize() {
    let n = NonZeroUsize::new(128).expect("128 is non-zero");
    let cap = PipeCapacity::Bounded(n);
    match cap {
        PipeCapacity::Bounded(inner) => assert_eq!(inner, n),
        _ => panic!("expected PipeCapacity::Bounded"),
    }
}

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

// ConfigError

/// `ConfigError::ZeroCapacity` is the variant used by every `*Capacity::bounded(0)`
/// — pinned down so the error path stays a single shared variant across the
/// three capacity types.
#[test]
fn config_error_has_zero_capacity_variant() {
    let err = ConfigError::ZeroCapacity;
    assert!(matches!(err, ConfigError::ZeroCapacity));
}

/// `ConfigError::InvalidRestartWithin` is the variant returned by
/// `SupervisionStrategy::restart(_, Duration::ZERO)`. Asserting the variant
/// exists here keeps the dependency between Capacity tests and Supervision
/// tests visible (they share `ConfigError`).
#[test]
fn config_error_has_invalid_restart_within_variant() {
    let err = ConfigError::InvalidRestartWithin;
    assert!(matches!(err, ConfigError::InvalidRestartWithin));
}

/// `ConfigError` implements `std::error::Error` so it can flow through
/// `thiserror`-derived parent enums without extra wrapping.
#[test]
fn config_error_implements_error_trait() {
    fn assert_error<E: std::error::Error>(_e: &E) {}
    let err = ConfigError::ZeroCapacity;
    assert_error(&err);
}
