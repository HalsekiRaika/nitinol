//! The saga builder must lose its codec typestate along with the rest of the
//! codec injection API.
//!
//! A saga's journal is written in `S::Event` and its upstream is read as the
//! event types the external variants of `S::Subscribe` name; all of them are
//! `Event`, so all of them already name the codec that reads and writes them.
//! Keeping a `with_codec` step here would leave a way to journal a saga in a
//! format other than the one its own event type named.
//!
//! The contract is observed the way `saga_snapshot_type_removed.rs` observes
//! its own: the crate's public namespace and a local `probe` namespace are both
//! glob-imported and the bare names are used. Once the crate stops exporting
//! them the names have a single source and resolve to the probe; while it still
//! exports them the two globs collide and this test binary fails to build. The
//! probe definitions carry a field the released types never had, so resolving
//! the ambiguity in the crate's favour does not rescue the build either.
//!
//! Removing `CodecSet` settles the method as well as the marker:
//! `SagaManagerProps::with_codec` — on the one saga builder there is, since a
//! saga whose `correlate` always answers the same id is the degenerate case of
//! the manager — exists only to return `..<S, CodecSet<S::Event>, ..>`, so it
//! cannot survive a type that cannot be named.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_codec_injection_api_removed`.

/// What a downstream crate is free to define once the names are released.
mod probe {
    pub struct CodecUnset {
        pub owner: &'static str,
    }

    pub struct CodecSet {
        pub owner: &'static str,
    }
}

// The crate glob is the probe, not a source of names: it contributes nothing
// only while the crate exports neither marker.  The moment it exports one, this
// glob collides with `probe::*` and the use sites below stop resolving.
#[allow(unused_imports)]
use nitinol_saga::*;
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";

/// Given both namespaces are glob-imported, When the bare `CodecUnset` and
/// `CodecSet` names are used, Then they denote the downstream probe
/// definitions — the saga builder no longer steps through a codec stage.
#[test]
fn the_saga_codec_typestate_names_belong_to_downstream_definitions() {
    let unset = CodecUnset { owner: PROBE_OWNER };
    let set = CodecSet { owner: PROBE_OWNER };

    assert_eq!(
        (unset.owner, set.owner),
        (PROBE_OWNER, PROBE_OWNER),
        "`CodecSet` / `CodecUnset` must not be exported by `nitinol-saga` any more"
    );
}
