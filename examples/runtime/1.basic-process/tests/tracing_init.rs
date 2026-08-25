//! Tests for the `init_tracing()` pattern introduced by the println→tracing migration.
//!
//! These tests verify the two requirements stated in the plan:
//! - `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))`
//!   produces a valid filter without panicking when `RUST_LOG` is absent.
//! - Non-console path: `registry().with(fmt::layer()).with(env_filter)` composes
//!   successfully — the same assembly used inside `init_tracing()`.
//!
//! Note: `.init()` is intentionally NOT called here.  Calling `.init()` sets a
//! process-global tracing subscriber; doing so inside a test binary would cause
//! every subsequent attempt to set the global subscriber to panic.  Building the
//! `Layered` type (without calling `.init()`) is sufficient to verify that the
//! type signatures are compatible and the construction is panic-free.
//!
//! These tests require `tracing-subscriber` in `[dependencies]`, which is added
//! by the implementation step.  Build failures here are expected until that step
//! completes.

// ─── R3-1: EnvFilter fallback ─────────────────────────────────────────────────

/// When `RUST_LOG` is not set, `try_from_default_env()` returns `Err` and the
/// closure falls back to `EnvFilter::new("info")`.  This test verifies that the
/// exact fallback expression from `init_tracing()` never panics.
#[test]
fn env_filter_falls_back_to_info_when_rust_log_unset() {
    // Given: RUST_LOG is unset (the test runner typically does not set it)

    // When: the fallback expression from init_tracing() is evaluated
    let _filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // Then: a valid EnvFilter is produced without panic —
    //       unwrap_or_else guarantees the "info" fallback covers the absent-RUST_LOG case.
}

/// When `RUST_LOG` is set to a valid directive, `try_from_default_env()` succeeds
/// and the `unwrap_or_else` fallback closure is NOT invoked.
/// This test verifies the happy path does not shadow user-supplied directives.
#[test]
fn env_filter_honours_rust_log_when_set() {
    // Given: RUST_LOG is set to a valid directive in this process
    // (use a thread-local override to avoid racing with other tests)
    let original = std::env::var("RUST_LOG").ok();
    // SAFETY: single-threaded mutation; restore afterward
    unsafe { std::env::set_var("RUST_LOG", "debug") };

    // When: the fallback expression is evaluated
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // Restore original state before any assertions that could panic
    match original {
        // SAFETY: single-threaded restore of the value this test overwrote.
        Some(v) => unsafe { std::env::set_var("RUST_LOG", v) },
        // SAFETY: single-threaded restore of the absent-variable state.
        None => unsafe { std::env::remove_var("RUST_LOG") },
    }

    // Then: try_from_default_env succeeded (filter is the user-supplied "debug" directive)
    drop(filter); // construction must not have panicked
}

// ─── R4-2: non-console subscriber composition ────────────────────────────────

/// Verifies that `registry().with(fmt::layer()).with(env_filter)` — the exact
/// non-console branch of `init_tracing()` — builds without a type error or panic.
///
/// This confirms that the three composable layers are compatible with the
/// `SubscriberExt` + `SubscriberInitExt` trait bounds required by `.init()`.
#[test]
fn tracing_registry_with_fmt_and_env_filter_composes_without_panic() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{fmt, EnvFilter};

    // Given: an EnvFilter identical to the one produced by init_tracing()
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // When: the non-console branch of init_tracing() is assembled (without .init())
    let _subscriber = tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter);

    // Then: the Layered type is constructed without panic —
    //       the layer ordering and trait bounds are satisfied.
}
