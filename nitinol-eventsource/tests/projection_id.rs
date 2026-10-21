//! Where a projection's identifier lives, and the key it renders.
//!
//! ```gherkin
//!   Scenario: the identifier is the projector's neighbour
//!     Given code naming nitinol_eventsource::ProjectionId
//!     And code naming nitinol_eventsource::projection::ProjectionId
//!     Then both name one type
//!
//!   Scenario: the reserved namespace is refused here too
//!     Given a name inside the reserved namespace
//!     When a ProjectionId is constructed from it
//!     Then the construction is refused
//!
//!   Scenario: a projection's offset key is its identifier, verbatim
//!     Given a ProjectionId
//!     When an OffsetScope is made from it
//!     Then as_str answers with the identifier's own text
//!     And it is the key a user-named scope of the same text renders
//! ```
//!
//! The identifier belongs beside the projector rather than beside the stores:
//! `AggregateId` is in `nitinol-persistence` because the journal keys its
//! streams by it, and `SagaId` is in `nitinol-saga` for the same reason — a
//! projection's identifier names a consumer this crate owns, and the offset key
//! it renders is made by a conversion rather than by the store knowing the type.
//!
//! The refusal is `nitinol_persistence::reject_reserved_id`'s, the same one
//! `AggregateId` and `SagaId` route through, so all of them refuse the same
//! names and say so the same way. What is fixed here is that moving the
//! definition did not leave the rule behind.
//!
//! Run with: `cargo test -p nitinol-eventsource --test projection_id`.

use nitinol_eventsource::projection::ProjectionId as ProjectionIdBesideTheProjector;
use nitinol_eventsource::ProjectionId;
use nitinol_persistence::store::OffsetScope;

/// Names a projection may carry: outside the reserved namespace, including the
/// ones a naive prefix test would wrongly claim.
const NAMES_A_PROJECTION_MAY_CARRY: &[&str] = &[
    "order-totals",
    "nitinolx",
    "nitinol-1",
    "nitinol_saga",
    "my.nitinol",
    "order-7",
    "",
];

/// Names inside the reserved namespace, which belongs to framework-owned keys.
const NAMES_THE_FRAMEWORK_RESERVED: &[&str] =
    &["nitinol", "nitinol.projection", "nitinol.saga.dead_letter"];

/// Both paths name one type, so a user reaching the identifier through the
/// module it is defined in and one reaching it through the crate root hold the
/// same thing. The annotated binding is the whole assertion: it compiles only
/// where the two paths are one type.
#[test]
fn the_identifier_is_reached_through_the_projection_module_and_the_crate_root() {
    let through_the_root = ProjectionId::new("order-totals");
    let through_the_module: ProjectionIdBesideTheProjector = through_the_root.clone();

    assert_eq!(
        through_the_module.as_str(),
        through_the_root.as_str(),
        "`nitinol_eventsource::ProjectionId` and \
         `nitinol_eventsource::projection::ProjectionId` must be one type: the \
         root re-export is a second way in, not a second identifier"
    );
}

#[test]
fn the_identifier_refuses_a_name_inside_the_reserved_namespace() {
    for name in NAMES_THE_FRAMEWORK_RESERVED {
        let named_as = (*name).to_owned();
        let outcome = std::panic::catch_unwind(move || ProjectionId::new(named_as));

        let panicked = outcome.expect_err(&format!(
            "a projection named {name:?} must be refused: the identifier renders \
             its offset key verbatim, so one inside the reserved namespace could \
             render the key of a framework-owned consumer"
        ));
        let message = panicked
            .downcast_ref::<String>()
            .expect("the shared identifier refusal panics with a formatted String");
        assert!(
            message.contains("reserved namespace") && message.contains(name),
            "the refusal must be identifiable as a reserved-namespace violation \
             and name the value that violated it, got {message:?}"
        );
    }
}

/// The refusal must not spill over onto identifiers that merely share a prefix
/// with the reserved root, nor onto ordinary ones.
#[test]
fn the_identifier_accepts_every_name_outside_the_reserved_namespace() {
    for name in NAMES_A_PROJECTION_MAY_CARRY {
        assert_eq!(
            ProjectionId::new(*name).as_str(),
            *name,
            "a ProjectionId must accept {name:?} unchanged"
        );
    }
}

/// A projection's key is the identifier's own text: a stored row is found under
/// it, so the conversion must neither prefix nor decorate it.
#[test]
fn a_projections_offset_key_is_its_identifier_verbatim() {
    for name in NAMES_A_PROJECTION_MAY_CARRY {
        let scope = OffsetScope::from(&ProjectionId::new(*name));

        assert_eq!(
            scope.as_str(),
            *name,
            "the key a projection's offset is filed under is the identifier \
             itself; a rendering that added anything would move every row the \
             projection had already written"
        );
        assert_eq!(
            scope,
            OffsetScope::new(*name),
            "and it is the key the user-naming constructor renders for the same \
             text — the conversion goes through that constructor rather than \
             rendering a key of its own"
        );
    }
}
