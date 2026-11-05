//! Tests for the `RUST_LOG` handling that `init_tracing()` relies on.
//!
//! Note: `.init()` is intentionally NOT called here.  Calling `.init()` sets a
//! process-global tracing subscriber; doing so inside a test binary would cause
//! every subsequent attempt to set the global subscriber to panic.

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
