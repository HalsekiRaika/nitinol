//! `spawn_scheduler` must be gone from the crate's public namespace.
//!
//! The scheduler is now started by type —
//! `ProcessSystem::spawn_resident::<SchedulerProcess>()`, pinned positively in
//! `saga_scheduler_resident.rs` — and a surviving spawn function would be a
//! second way in that starts a timer core per call instead of resolving the one
//! the system holds.
//!
//! A name that no longer exists cannot be imported, so the contract is observed
//! the way `saga_snapshot_type_removed.rs` and
//! `saga_codec_injection_api_removed.rs` observe theirs: the crate's public
//! namespace and a local `probe` namespace are both glob-imported and the bare
//! name is used.
//!
//! - Once the crate stops exporting the function, the bare name has a single
//!   source and resolves to the probe.
//! - While the crate still exports it, the two globs collide on the name and
//!   the use below is rejected as ambiguous, so this test binary fails to build.
//! - Resolving that ambiguity in the crate's favour does not rescue the build
//!   either: the crate's function is `async` and takes a `&ProcessSystem`, so
//!   neither the argument below nor the un-awaited use of its result type-checks.
//!
//! A definition that is only hidden — kept in the crate but dropped from the
//! re-export — is invisible here; that residue is caught by the crate's own
//! dead-code lint rather than by a downstream build.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_spawn_scheduler_api_removed`.

/// What a downstream crate is free to define once the name is released: a host
/// that already holds its timer core naming it for its own wiring.
mod probe {
    pub struct SchedulerHandle {
        pub label: &'static str,
    }

    pub fn spawn_scheduler(label: &'static str) -> SchedulerHandle {
        SchedulerHandle { label }
    }
}

// The crate glob is the probe, not a source of names: it contributes nothing
// only while the crate exports no `spawn_scheduler`.  The moment it exports one,
// this glob collides with `probe::*` and the call below stops resolving.
#[allow(unused_imports)]
use nitinol_saga::*;
use probe::*;

const PROBE_LABEL: &str = "downstream-probe";

/// Given both namespaces are glob-imported, when the bare `spawn_scheduler`
/// name is called, then it denotes the downstream probe definition.
#[test]
fn the_spawn_scheduler_name_belongs_to_downstream_definitions() {
    let handle = spawn_scheduler(PROBE_LABEL);

    assert_eq!(
        handle.label, PROBE_LABEL,
        "`spawn_scheduler` must not be exported by `nitinol-saga` any more"
    );
}
