//! `SagaSnapshot` must be gone from the crate's public namespace.
//!
//! A name that no longer exists cannot be imported, so the contract is
//! observed by letting a downstream definition claim the name: the crate's
//! public namespace and a local `probe` namespace are both glob-imported, and
//! the test builds the bare `SagaSnapshot`.
//!
//! - Once the crate stops exporting the type, the bare name has a single
//!   source and resolves to the probe.
//! - While the crate still exports it, the two globs collide on the name and
//!   the use is rejected as ambiguous, so this test binary fails to build.
//! - Resolving that ambiguity in the crate's favour does not rescue the build
//!   either: the crate's placeholder is `#[non_exhaustive]` and fieldless, so
//!   the struct literal below cannot name it.
//!
//! A definition that is only hidden — kept in the crate but dropped from the
//! re-export — is invisible here; that residue is caught by the crate's own
//! dead-code lint rather than by a downstream build.

mod probe {
    use nitinol_saga::SagaId;

    /// What a downstream crate is free to define once the name is released:
    /// a snapshot value that names the saga it belongs to.
    pub struct SagaSnapshot {
        pub owner: SagaId,
        pub tag: &'static str,
    }
}

use nitinol_saga::*;
use probe::*;

const PROBE_TAG: &str = "downstream-probe";

/// Given both namespaces are glob-imported, when the bare `SagaSnapshot` name
/// is used, then it denotes the downstream probe definition.
#[test]
fn saga_snapshot_name_belongs_to_downstream_definitions() {
    let snapshot = SagaSnapshot {
        owner: SagaId::new("snapshot-owner"),
        tag: PROBE_TAG,
    };

    assert_eq!(
        (snapshot.owner.as_str(), snapshot.tag),
        ("snapshot-owner", PROBE_TAG),
        "`SagaSnapshot` must not be exported by the crate any more"
    );
}
