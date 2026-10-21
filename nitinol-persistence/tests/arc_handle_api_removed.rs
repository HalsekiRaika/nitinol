//! Store handles are distributed by the backend's `Clone`, so the framework's
//! `Arc` layer leaves no surface behind.
//!
//! ```gherkin
//!   Scenario: the erased store type is named `Journal`
//!     Given user code naming DynStore
//!     When it is compiled against this crate
//!     Then compilation fails — the journal-only store is `Journal` itself, and
//!          nothing is deprecated or kept beside it
//!
//!   Scenario: an `Arc` around a store is not a store
//!     Given an Arc<InMemoryEventStore> / Arc<InMemorySnapshotStore> /
//!           Arc<InMemoryOffsetStore>
//!     When a store trait's verb is called on it
//!     Then the crate claims none of those names for it
//!
//!   Scenario: a supplier answers with handles rather than with shared ones
//!     Given the four supplier accessors
//!     When their signatures are named
//!     Then the journal is a `Journal`, the snapshots an `Album`, and each
//!          offset slot the backend's own handle — none of them behind an `Arc`
//!
//!   Scenario: an assembled store's type names the erased handles
//!     Given a store assembled over a backend journal and a backend snapshot
//!           store
//!     When the assembled store's own type is named
//!     Then it has no parameter for the journal, and the snapshot slot's is
//!          `Album` rather than the backend the slot was named with
//! ```
//!
//! What the `Arc` stood for was identity, and identity is the store's own
//! obligation: `PRV-1` is kept by the backend whose handle was registered, and
//! `PRV-2` says that a shared pointer is neither necessary nor sufficient for
//! it. A framework that wrapped a handle anyway proved the same property twice,
//! and the inner proof — `InMemoryEventStore`'s own state, a pool a client
//! cloned — is the one that was load-bearing. So the blanket implementations
//! that made `Arc<T>` a store in its own right are gone, and with them the
//! `Arc<dyn EventStore>` the wiring used to be written against.
//!
//! The clauses that read *rows* are positive and live in
//! `persistence_provider.rs`. What is settled here is that the withdrawn
//! surface is withdrawn, by the two mechanisms the `*_api_removed.rs` tests in
//! this workspace already use:
//!
//! 1. **Released type names** — the crate's public namespaces and a local
//!    `probe` namespace are both glob-imported and the bare name is used. Once
//!    the crate stops exporting it the name has a single source and resolves to
//!    the probe; while it still exports one the two globs collide and this
//!    binary fails to build. The probe type carries an `owner` field the
//!    released type never had, so resolving the ambiguity in the crate's favour
//!    does not rescue the build either.
//! 2. **Released trait implementations** — a blanket extension trait claims the
//!    store verbs with a signature the store traits could never accept. While
//!    the crate still implements any of the three store traits for a shared
//!    pointer, a call on `Arc<InMemoryEventStore>` has two applicable items in
//!    scope and fails to resolve. Once those blankets are gone, the extension is
//!    the only candidate and resolves.
//!
//! Run with: `cargo test -p nitinol-persistence --test arc_handle_api_removed`.

use std::sync::Arc;

/// What a downstream crate is free to define once the names are released.
mod probe {
    /// What a released name means to a downstream crate: nothing the framework
    /// decides. The `owner` field is what no released type ever had.
    macro_rules! released_type {
        ($($name:ident),+ $(,)?) => {
            $(
                pub struct $name {
                    pub owner: &'static str,
                }
            )+
        };
    }

    released_type!(DynStore);

    /// What a probed call answers with once the framework stops claiming the
    /// name for an `Arc`.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims every store verb the withdrawn `Arc` blanket implementations used
    /// to supply.
    ///
    /// One trait for all three stores: the three blankets were one decision —
    /// that the framework's shared pointer is a handle in its own right — and
    /// they are withdrawn as one.
    pub trait NoStoreTraitsForArc {
        fn append(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn load(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn save(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn load_latest(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoStoreTraitsForArc for T {}
}

// The crate globs are the probe, not a source of names: each contributes
// nothing only while the crate exports none of the released names, and brings
// the store traits into scope so that a surviving blanket is an ambiguity at
// the use sites below rather than a silent pass.
#[allow(unused_imports)]
use nitinol_persistence::provider::*;
#[allow(unused_imports)]
use nitinol_persistence::store::*;
// A glob, not a named import: a named import would shadow the crate's export
// instead of colliding with it, and the collision is the whole mechanism.
#[allow(unused_imports)]
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";
const PROBE_LABEL: &str = "downstream-label";

/// Given both namespaces are glob-imported, When `DynStore` is used, Then it
/// denotes the downstream probe definition.
#[test]
fn the_erased_store_type_name_belongs_to_a_downstream_definition() {
    assert_eq!(
        DynStore { owner: PROBE_OWNER }.owner,
        PROBE_OWNER,
        "`nitinol-persistence` must not export `DynStore`: a store holding the \
         journal alone is `Journal` itself, which is what the erased system \
         handle is fixed on, so a second type naming the same thing would be a \
         wrapper whose clone did not reach the backend's"
    );
}

/// Given a blanket extension trait that claims the store verbs, When each is
/// called on an `Arc` around a reference store, Then every call resolves to the
/// extension.
///
/// An `Arc` around a handle is a shared pointer and nothing more. While the
/// crate claimed otherwise, wrapping a handle was the idiom for naming one
/// twice — and that is what put the identity proof in two places.
#[test]
fn the_store_verbs_of_an_arc_belong_to_downstream_definitions() {
    let events = Arc::new(InMemoryEventStore::default());
    let snapshots = Arc::new(InMemorySnapshotStore::default());
    let offsets = Arc::new(InMemoryOffsetStore::default());

    let answered = [
        events.append(PROBE_LABEL).label,
        events.load(PROBE_LABEL).label,
        snapshots.save(PROBE_LABEL).label,
        snapshots.load_latest(PROBE_LABEL).label,
        offsets.load(PROBE_LABEL).label,
        offsets.save(PROBE_LABEL).label,
    ];

    assert!(
        answered.iter().all(|label| *label == PROBE_LABEL),
        "an `Arc` around a store must not be a store: distribution is the \
         backend's own `Clone`, carried through the erasure by `Journal` and \
         `Album`, so nothing in the framework has a reason to wrap a handle; \
         got {answered:?}"
    );
}

/// The four supplier accessors answer with handles of their own.
///
/// Not a `#[test]` — the compiler enforces the signatures at build time. A
/// return to a shared handle (`Arc<dyn EventStore>`, `Arc<S::ProjectionOffsets>`)
/// is a compile error here, and so is a journal that stayed the backend's own
/// type instead of being erased.
#[allow(dead_code)]
fn _assert_the_supplier_answers_with_owned_handles() {
    let _: fn(&PersistenceProvider<InMemoryStore>) -> Journal =
        PersistenceProvider::<InMemoryStore>::event_store;
    let _: fn(&PersistenceProvider<InMemoryStore>) -> Album =
        PersistenceProvider::<InMemoryStore>::snapshot_store;
    let _: fn(&PersistenceProvider<InMemoryStore>) -> InMemoryOffsetStore =
        PersistenceProvider::<InMemoryStore>::projection_offsets;
    let _: fn(&PersistenceProvider<InMemoryStore>) -> InMemoryOffsetStore =
        PersistenceProvider::<InMemoryStore>::saga_offsets;
}

/// A store answers for its slots with the erased handles too, which is what
/// lets one type name every deployment's journal.
#[allow(dead_code)]
fn _assert_a_store_answers_with_erased_handles() {
    let _: fn(&InMemoryStore) -> Journal = <InMemoryStore as Store>::events;
    let _: fn(&InMemoryStore) -> Album = <InMemoryStore as SnapshotSlot>::snapshots;
}

/// An assembled store's own type names the erased handles, not the backends it
/// was assembled over.
///
/// Not a `#[test]` — the compiler enforces the type at build time. A parameter
/// for the journal, or a snapshot parameter that stayed the backend's own type,
/// is a compile error here: every alias and signature that mentions an
/// assembled store would otherwise have to write the backend's type again,
/// which is the carrying-through the erasure exists to end. The offset slots
/// keep the backend's type on purpose, because `OffsetStore::Tx` has to stay
/// visible to a projector settling its update and its position together.
#[allow(dead_code)]
fn _assert_an_assembled_stores_type_names_the_erased_handles() {
    let _: CustomStore<Album, InMemoryOffsetStore, InMemoryOffsetStore> = CustomStore::builder()
        .eventstore(InMemoryEventStore::default())
        .snapshot(InMemorySnapshotStore::default())
        .offsets(InMemoryOffsetStore::default())
        .build();
}

/// The erased handles are values the framework can hold, hand on and keep
/// across a spawn without anything around them.
#[allow(dead_code)]
fn _assert_the_erased_handles_are_shareable_values() {
    fn shareable<T: Clone + std::fmt::Debug + Send + Sync + 'static>() {}

    shareable::<Journal>();
    shareable::<Album>();
}
