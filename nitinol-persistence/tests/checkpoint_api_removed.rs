//! The checkpoint vocabulary leaves no surface behind: a user who still names
//! one of its types or verbs must fail to compile.
//!
//! ```gherkin
//!   Scenario: a reference to a removed name is a compile error
//!     Given user code naming CheckpointScope / CheckpointStore /
//!           CheckpointError / InMemoryCheckpointStore /
//!           ProjectionCheckpointSlot / DeliveryCheckpointSlot / ProjectionId /
//!           projection_checkpoints / delivery_checkpoints / checkpoints
//!     When it is compiled against this crate
//!     Then compilation fails — nothing is deprecated and no hidden compatible
//!          shim answers for the name
//! ```
//!
//! What this store holds is a position in an order, which is an *offset* and
//! not a checkpoint: in event sourcing a checkpoint is also the word for a
//! saved state, and a reader who took it that way would look for a snapshot.
//! The offset names are pinned positively by `offset_store.rs`,
//! `offset_scope.rs` and `persistence_provider.rs`; this file pins that the
//! names they replace are gone rather than kept beside them.
//!
//! `ProjectionId` is here for a different reason: it moved rather than being
//! renamed. It names a consumer `nitinol-eventsource` owns, and it is reached
//! through `nitinol_eventsource::ProjectionId` — pinned by that crate's
//! `projection_id.rs`.
//!
//! `CheckpointScope::checkpoint_key` needs no clause of its own: the trait was
//! the only thing that declared it, so a probe that settles the trait name
//! settles the method with it.
//!
//! Two mechanisms, each the one that fits what is being settled, both taken
//! from the `*_api_removed.rs` tests already in this workspace:
//!
//! 1. **Released type names** — the crate's public namespaces and a local
//!    `probe` namespace are both glob-imported and the bare names are used.
//!    Once the crate stops exporting them the names have a single source and
//!    resolve to the probe; while it still exports one the two globs collide
//!    and this binary fails to build. Each probe type carries an `owner` field
//!    the released types never had, so resolving the ambiguity in the crate's
//!    favour does not rescue the build either.
//! 2. **Released methods** — a blanket extension trait claims the name with a
//!    signature the released method could never accept. The released ones are
//!    *inherent* methods, and an inherent method wins method resolution over a
//!    trait one, so while it exists the call below is checked against **it** and
//!    fails on its arguments. Once the name is gone, the blanket method is the
//!    only candidate and resolves.
//!
//! Run with: `cargo test -p nitinol-persistence --test checkpoint_api_removed`.

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

    released_type!(
        CheckpointScope,
        CheckpointStore,
        CheckpointError,
        InMemoryCheckpointStore,
        ProjectionCheckpointSlot,
        DeliveryCheckpointSlot,
        ProjectionId,
    );

    /// What a probed call answers with once the framework stops claiming the
    /// name.
    pub struct Declined {
        pub label: &'static str,
    }

    /// Claims the two withdrawn supplier accessors.
    pub trait NoCheckpointSlotAccessors {
        fn projection_checkpoints(&self, label: &'static str) -> Declined {
            Declined { label }
        }

        fn delivery_checkpoints(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoCheckpointSlotAccessors for T {}

    /// Claims the withdrawn builder verb that named both slots with one handle.
    pub trait NoCheckpointsBuilderVerb {
        fn checkpoints(&self, label: &'static str) -> Declined {
            Declined { label }
        }
    }

    impl<T> NoCheckpointsBuilderVerb for T {}
}

// The crate globs are the probe, not a source of names: each contributes
// nothing only while the crate exports none of the released names. The moment
// it exports one, that glob collides with `probe::*` and the use sites below
// stop resolving.
#[allow(unused_imports)]
use nitinol_persistence::error::*;
#[allow(unused_imports)]
use nitinol_persistence::provider::*;
#[allow(unused_imports)]
use nitinol_persistence::store::*;
#[allow(unused_imports)]
use nitinol_persistence::*;
// A glob, not a named import: a named import would shadow the crate's export
// instead of colliding with it, and the collision is the whole mechanism.
#[allow(unused_imports)]
use probe::*;

const PROBE_OWNER: &str = "downstream-probe";
const PROBE_LABEL: &str = "downstream-label";

/// Given both namespaces are glob-imported, When each bare checkpoint name is
/// used, Then every one of them denotes the downstream probe definition.
#[test]
fn the_checkpoint_type_names_belong_to_downstream_definitions() {
    let owners = [
        CheckpointScope { owner: PROBE_OWNER }.owner,
        CheckpointStore { owner: PROBE_OWNER }.owner,
        CheckpointError { owner: PROBE_OWNER }.owner,
        InMemoryCheckpointStore { owner: PROBE_OWNER }.owner,
        ProjectionCheckpointSlot { owner: PROBE_OWNER }.owner,
        DeliveryCheckpointSlot { owner: PROBE_OWNER }.owner,
    ];

    assert!(
        owners.iter().all(|owner| *owner == PROBE_OWNER),
        "every checkpoint name must be released by `nitinol-persistence`: what \
         the store holds is a position in an order, so the contract, its error, \
         its reference implementation, its key and both provider slots are \
         named for an offset; got {owners:?}"
    );
}

/// Given the same two namespaces, When `ProjectionId` is used, Then it denotes
/// the downstream definition — the identifier moved to `nitinol-eventsource`,
/// beside the projector it names.
#[test]
fn the_projection_identifier_name_belongs_to_a_downstream_definition() {
    assert_eq!(
        ProjectionId { owner: PROBE_OWNER }.owner,
        PROBE_OWNER,
        "`nitinol-persistence` must not export `ProjectionId`: it was here \
         because the offset store's key type had to be, and the key is a \
         concrete `OffsetScope` now — so the identifier lives beside the \
         projector that names it"
    );
}

/// Given a blanket extension trait that claims the two withdrawn supplier
/// accessors, When each is called on a supplier whose store holds both slots,
/// Then both resolve to the extension.
#[test]
fn the_withdrawn_slot_accessor_names_belong_to_downstream_definitions() {
    let provider = PersistenceProvider::new(InMemoryStore::default());

    assert_eq!(
        (
            provider.projection_checkpoints(PROBE_LABEL).label,
            provider.delivery_checkpoints(PROBE_LABEL).label,
        ),
        (PROBE_LABEL, PROBE_LABEL),
        "a supplier must distribute `projection_offsets` and `saga_offsets`: \
         the second slot is the one every saga attachment records in, not the \
         delivery host's alone"
    );
}

/// Given a blanket extension trait that claims the withdrawn builder verb, When
/// it is called on a store builder, Then it resolves to the extension —
/// naming both slots with one handle is `offsets`.
#[test]
fn the_withdrawn_builder_verb_belongs_to_a_downstream_definition() {
    assert_eq!(
        CustomStore::builder().checkpoints(PROBE_LABEL).label,
        PROBE_LABEL,
        "an assembly must name both slots with one handle through `offsets`: \
         the single-table deployment is still one verb, under the vocabulary \
         the slots carry"
    );
}
