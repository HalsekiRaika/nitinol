//! What an `EventRegistry` answers for, and what it refuses to be built from.
//!
//! ```gherkin
//!   Scenario: a collected event type resolves from the path it is stored under
//!     Given one entry per declaring type, of every shape an event type has
//!     When a registry is built from them
//!     Then each canonical path resolves to the event type it was declared as
//!
//!   Scenario: a canonical path two declarations both claim
//!     Given two types declaring one family and one type name between them
//!     When a registry is built from both
//!     Then the build fails, naming the path and every declaration that claimed
//!          it
//!
//!   Scenario: one declaration reaching the registry twice
//!     Then the build fails the same way
//!
//!   Scenario: a struct and an enum a shorter family would once have collided
//!     Then both resolve — the grammar leaves them no path to share
//! ```
//!
//! These clauses judge the validation a registry is built through, which is
//! the whole of what the process-wide registry adds to the entries the linker
//! collected. Nothing here reaches the linker: entries are handed over by hand
//! so a clause about a collision can live beside a clause that requires none,
//! which entries placed in the process-wide slice could not.
//!
//! Handing entries over is what `__private::event_registry_from_entries` is
//! for, and it is `__private` because it is not a user surface: the validation
//! it runs is the framework's defence layer, reachable here so the layer can be
//! judged on its own and reachable by a conformance suite for the same reason.
//!
//! A declaration is written down as one canonical path, so an entry carries one
//! event type.

use nitinol_persistence::__private::event_registry_from_entries;
use nitinol_persistence::{EventRegistry, EventRegistryEntry, EventType, Family, TypeName};

// Every shape an event type has: no family, a deep family, a one-segment family.

const BARE: EventType = EventType::new(Family::new(""), TypeName::new("Bare"));
const NESTED: EventType = EventType::new(
    Family::new("persistence.registry.deep"),
    TypeName::new("Nested"),
);
const CHOICE: EventType =
    EventType::new(Family::new("persistence.registry"), TypeName::new("Choice"));

static BARE_ENTRY: EventRegistryEntry = EventRegistryEntry::new("event_registry::Bare", &[BARE]);
static NESTED_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::Nested", &[NESTED]);
static CHOICE_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::Choice", &[CHOICE]);

fn shapes() -> Result<EventRegistry, nitinol_persistence::EventRegistryError> {
    event_registry_from_entries([&BARE_ENTRY, &NESTED_ENTRY, &CHOICE_ENTRY])
}

#[test]
fn every_shape_of_event_type_resolves_from_the_path_it_is_stored_under() {
    let registry = shapes().expect("entries claiming distinct canonical paths build a registry");

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
        registry.resolve("persistence.registry.Choice"),
        Some(CHOICE),
        "a declaration is stored under one path: its family and its type name"
    );
}

#[test]
fn a_path_no_entry_claimed_resolves_to_nothing() {
    let registry = shapes().expect("entries claiming distinct canonical paths build a registry");

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

// The collision a shorter family used to make possible. A type name is one
// segment, so `a.b.C` is read as family `a.b` and type name `C` and as nothing
// else: a type `b` of family `a` is written down as `a.b`, a path of its own.

const STRUCT_C_OF_A_B: EventType = EventType::new(Family::new("a.b"), TypeName::new("C"));
const TYPE_B_OF_A: EventType = EventType::new(Family::new("a"), TypeName::new("b"));

static STRUCT_C_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::grammar::C", &[STRUCT_C_OF_A_B]);
static TYPE_B_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::grammar::B", &[TYPE_B_OF_A]);

#[test]
fn a_deeper_family_and_a_shorter_one_claim_paths_of_their_own() {
    assert_ne!(
        STRUCT_C_OF_A_B.to_path().to_string(),
        TYPE_B_OF_A.to_path().to_string(),
        "a type name is one segment, so the two declarations decompose differently and cannot \
         share a path"
    );

    let registry = event_registry_from_entries([&STRUCT_C_ENTRY, &TYPE_B_ENTRY])
        .expect("declarations the grammar keeps apart build a registry together");

    assert_eq!(registry.resolve("a.b.C"), Some(STRUCT_C_OF_A_B));
    assert_eq!(registry.resolve("a.b"), Some(TYPE_B_OF_A));
}

// The collision that remains: two types declaring one family and one type name
// between them. Serving one of them would restore the other's rows as the wrong
// type, so the build is refused instead.

const CONTESTED: EventType = EventType::new(Family::new("a.b"), TypeName::new("Contested"));

const FIRST_DECLARER: &str = "event_registry::collision::First";
const SECOND_DECLARER: &str = "event_registry::collision::Second";

static FIRST_ENTRY: EventRegistryEntry = EventRegistryEntry::new(FIRST_DECLARER, &[CONTESTED]);
static SECOND_ENTRY: EventRegistryEntry = EventRegistryEntry::new(SECOND_DECLARER, &[CONTESTED]);

#[test]
fn a_canonical_path_two_declarations_claim_fails_the_build_and_names_both() {
    let Err(refusal) = event_registry_from_entries([&FIRST_ENTRY, &SECOND_ENTRY]) else {
        panic!(
            "one canonical path cannot answer for two event types, and the build was accepted — \
             whichever of them lost would have its rows restored as the other"
        )
    };

    let reported = refusal.to_string();
    assert!(
        reported.contains("a.b.Contested"),
        "the refusal names the canonical path that was claimed twice, and it reported {reported:?}"
    );
    assert!(
        reported.contains(FIRST_DECLARER) && reported.contains(SECOND_DECLARER),
        "the refusal names every declaration that claimed the path — a caller cannot find the two \
         definitions to rename without them — and it reported {reported:?}"
    );
}

#[test]
fn the_refusal_does_not_depend_on_the_order_the_entries_arrived_in() {
    let forwards = event_registry_from_entries([&FIRST_ENTRY, &SECOND_ENTRY])
        .expect_err("a canonical path claimed twice fails the build")
        .to_string();
    let backwards = event_registry_from_entries([&SECOND_ENTRY, &FIRST_ENTRY])
        .expect_err("a canonical path claimed twice fails the build")
        .to_string();

    assert_eq!(
        forwards, backwards,
        "the order entries reach the registry in is the linker's, which nothing in the source \
         fixes: a refusal that reads differently from one build to the next names a different \
         definition to go and fix each time"
    );
}

// Two contested paths at once. A refusal that stops at the first sends a caller
// back for a second build to be told about the second.

const FIRST_SHARED: EventType = EventType::new(Family::new("shared"), TypeName::new("First"));
const SECOND_SHARED: EventType = EventType::new(Family::new("shared"), TypeName::new("Second"));

static LEFT_FIRST: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::pair::LeftFirst", &[FIRST_SHARED]);
static RIGHT_FIRST: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::pair::RightFirst", &[FIRST_SHARED]);
static LEFT_SECOND: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::pair::LeftSecond", &[SECOND_SHARED]);
static RIGHT_SECOND: EventRegistryEntry =
    EventRegistryEntry::new("event_registry::pair::RightSecond", &[SECOND_SHARED]);

#[test]
fn every_path_more_than_one_declaration_claims_is_named_not_only_the_first() {
    let Err(refusal) =
        event_registry_from_entries([&LEFT_FIRST, &RIGHT_FIRST, &LEFT_SECOND, &RIGHT_SECOND])
    else {
        panic!("two contested paths cannot both be served")
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
    let Err(refusal) = event_registry_from_entries([&FIRST_LINK, &SECOND_LINK]) else {
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
