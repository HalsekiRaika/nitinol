//! What an `EventRegistry` answers for, and what it refuses to be built from.
//!
//! ```gherkin
//!   Scenario: a collected event type resolves from the path it is stored under
//!     Given entries declaring event types of every shape an event type has
//!     When a registry is built from them
//!     Then each canonical path resolves to the event type it was declared as
//!
//!   Scenario: a canonical path two declarations both claim
//!     Given a struct event `a.b::C` and arm `C` of `a::b`, declared apart
//!     When a registry is built from both
//!     Then the build fails, naming the path and every declaration that claimed
//!          it
//!
//!   Scenario: one declaration reaching the registry twice
//!     Then the build fails the same way
//! ```
//!
//! These clauses judge the validation a registry is built through, which is
//! the whole of what `EventRegistry::from_linked` adds to the entries the
//! linker collected. Nothing here reaches the linker: entries are handed over
//! by hand so a clause about a collision can live beside a clause that requires
//! none, which entries placed in the process-wide slice could not.

use nitinol_persistence::{
    EventRegistry, EventRegistryEntry, EventType, Family, TypeName, Variant,
};

// Every shape an event type has: no family, a deep family, an enum arm.

const BARE: EventType = EventType::new(Family::new(""), TypeName::new("Bare"));
const NESTED: EventType = EventType::new(
    Family::new("persistence.registry.deep"),
    TypeName::new("Nested"),
);
const ARM: EventType = EventType::with_variant(
    Family::new("persistence.registry"),
    TypeName::new("Choice"),
    Variant::new("Left"),
);

/// One declaration carrying every event type a single `#[derive(Event)]` would
/// place: the type-level identity and each arm of it.
static SHAPES: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::Shapes", &[BARE, NESTED, ARM]);

#[test]
fn every_shape_of_event_type_resolves_from_the_path_it_is_stored_under() {
    let registry = EventRegistry::from_entries([&SHAPES])
        .expect("entries claiming distinct canonical paths build a registry");

    assert_eq!(
        registry.resolve("Bare"),
        Some(BARE),
        "an event type with no family is stored under its type name alone"
    );
    assert_eq!(
        registry.resolve("persistence.registry.deep.Nested"),
        Some(NESTED),
        "an event type with a family is stored under family and type name"
    );
    assert_eq!(
        registry.resolve("persistence.registry.Choice.Left"),
        Some(ARM),
        "an enum arm is stored under the arm's path, variant included"
    );
}

#[test]
fn a_path_no_entry_claimed_resolves_to_nothing() {
    let registry = EventRegistry::from_entries([&SHAPES])
        .expect("entries claiming distinct canonical paths build a registry");

    assert_eq!(
        registry.resolve("persistence.registry.deep.Unregistered"),
        None,
        "a registry answers for the event types its entries declared and no others"
    );
    assert_eq!(
        registry.resolve(""),
        None,
        "the empty path is no event type's canonical path"
    );
}

// One canonical path, two readings of it: a struct event named `C` in family
// `a.b`, and arm `C` of type `b` in family `a`. Both are written down as
// `a.b.C`, so a registry that served one of them would restore the other's
// rows as the wrong type.

const STRUCT_C: EventType = EventType::new(Family::new("a.b"), TypeName::new("C"));
const ARM_C: EventType =
    EventType::with_variant(Family::new("a"), TypeName::new("b"), Variant::new("C"));

const STRUCT_DECLARER: &str = "event_registry::collision::StructC";
const ARM_DECLARER: &str = "event_registry::collision::ArmC";

static STRUCT_ENTRY: EventRegistryEntry = EventRegistryEntry::new(STRUCT_DECLARER, &[STRUCT_C]);
static ARM_ENTRY: EventRegistryEntry = EventRegistryEntry::new(ARM_DECLARER, &[ARM_C]);

#[test]
fn a_canonical_path_two_declarations_claim_fails_the_build_and_names_both() {
    assert_eq!(
        STRUCT_C.to_path().to_string(),
        ARM_C.to_path().to_string(),
        "the two readings share one canonical path, which is what makes the collision possible"
    );

    let Err(refusal) = EventRegistry::from_entries([&STRUCT_ENTRY, &ARM_ENTRY]) else {
        panic!(
            "one canonical path cannot answer for two event types, and the build was accepted — \
             whichever of them lost would have its rows restored as the other"
        )
    };

    let reported = refusal.to_string();
    assert!(
        reported.contains("a.b.C"),
        "the refusal names the canonical path that was claimed twice, and it reported {reported:?}"
    );
    assert!(
        reported.contains(STRUCT_DECLARER) && reported.contains(ARM_DECLARER),
        "the refusal names every declaration that claimed the path — a caller cannot find the two \
         definitions to rename without them — and it reported {reported:?}"
    );
}

#[test]
fn the_refusal_does_not_depend_on_the_order_the_entries_arrived_in() {
    let forwards = EventRegistry::from_entries([&STRUCT_ENTRY, &ARM_ENTRY])
        .expect_err("a canonical path claimed twice fails the build")
        .to_string();
    let backwards = EventRegistry::from_entries([&ARM_ENTRY, &STRUCT_ENTRY])
        .expect_err("a canonical path claimed twice fails the build")
        .to_string();

    assert_eq!(
        forwards, backwards,
        "the order entries reach the registry in is the linker's, which nothing in the source \
         fixes: a refusal that reads differently from one build to the next names a different \
         definition to go and fix each time"
    );
}

// Two declarations claiming two paths each. A refusal that stops at the first
// collision sends a caller back for a second build to be told about the second.

const FIRST_SHARED: EventType = EventType::new(Family::new("shared"), TypeName::new("First"));
const SECOND_SHARED: EventType = EventType::new(Family::new("shared"), TypeName::new("Second"));

static LEFT_PAIR: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::pair::Left", &[FIRST_SHARED, SECOND_SHARED]);
static RIGHT_PAIR: EventRegistryEntry = EventRegistryEntry::new(
    "event_registry::pair::Right",
    &[FIRST_SHARED, SECOND_SHARED],
);

#[test]
fn every_path_more_than_one_declaration_claims_is_named_not_only_the_first() {
    let Err(refusal) = EventRegistry::from_entries([&LEFT_PAIR, &RIGHT_PAIR]) else {
        panic!("two declarations claiming the same two paths cannot both be served")
    };

    let reported = refusal.to_string();
    assert!(
        reported.contains("shared.First") && reported.contains("shared.Second"),
        "a build fails once and reports every path it could not serve, so a caller fixes them in \
         one pass rather than one build per collision; it reported {reported:?}"
    );
}

// The same declaration reaching the registry twice — what a crate linked at two
// versions puts in the slice. Serving it would leave the registry's answer
// depending on which copy the linker happened to place last.

const TWICE: EventType =
    EventType::new(Family::new("persistence.registry"), TypeName::new("Twice"));
const TWICE_DECLARER: &str = "event_registry::duplicate::Twice";

static FIRST_LINK: EventRegistryEntry = EventRegistryEntry::new(TWICE_DECLARER, &[TWICE]);
static SECOND_LINK: EventRegistryEntry = EventRegistryEntry::new(TWICE_DECLARER, &[TWICE]);

#[test]
fn one_declaration_reaching_the_registry_twice_fails_the_build() {
    let Err(refusal) = EventRegistry::from_entries([&FIRST_LINK, &SECOND_LINK]) else {
        panic!(
            "a declaration collected twice is a duplicate registration, and the build was accepted"
        )
    };

    let reported = refusal.to_string();
    assert!(
        reported.contains("persistence.registry.Twice"),
        "the refusal names the canonical path that was registered twice, and it reported \
         {reported:?}"
    );
    assert!(
        reported.contains(TWICE_DECLARER),
        "the refusal names the declaration behind the duplicate, and it reported {reported:?}"
    );
}
