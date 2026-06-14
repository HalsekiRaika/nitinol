use std::time::Duration;

/// Controls the idle timeout behavior for a single process.
#[derive(Debug, Clone, Copy, Default)]
pub enum IdleTimeout {
    /// Inherit the system-level default idle timeout (no timeout if no system default is set).
    #[default]
    Inherit,
    /// Never time out regardless of the system default.
    Persistent,
    /// Time out after this duration of idle (no messages received).
    After(Duration),
}

/// Resolve an `IdleTimeout` policy against the system-level default to a
/// concrete `Option<Duration>`.
///
/// Defined here (and re-used by `SpawnEnv`) so child processes inherit the
/// system default consistently with top-level spawns.
pub(crate) fn resolve_idle_timeout(
    idle_timeout: IdleTimeout,
    default_idle_timeout: Option<Duration>,
) -> Option<Duration> {
    match idle_timeout {
        IdleTimeout::After(dur) => Some(dur),
        IdleTimeout::Persistent => None,
        IdleTimeout::Inherit => default_idle_timeout,
    }
}
