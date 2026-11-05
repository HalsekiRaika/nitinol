//! What the linker collects, without anyone registering anything by hand.
//!
//! ```gherkin
//!   Scenario: an entry placed in the slice is collected
//!     Given entries declared in a crate this binary links
//!     When a registry is built from what was linked
//!     Then every canonical path those entries declared resolves
//!
//!   Scenario: a path no linked entry declares
//!     Then it resolves to nothing — collection is what the linker found, not
//!          everything that could exist
//!
//!   Scenario: the registry a process shares
//!     Given the registry has already been handed out once
//!     When it is asked for again
//!     Then the same one is handed out — it is built once for the process, and
//!          no caller builds one of its own
//! ```
//!
//! The entries below stand for what `#[derive(Event)]` writes: one entry per
//! declaring type, carrying the one canonical path that type is written down
//! as. A hand-written `impl Event` places the same shape itself, which is why
//! the entry type and the slice are public rather than hidden behind the derive.
//!
//! Reaching the registry is the framework's own business: a store holds what
//! `linked_event_registry` hands it rather than building one, which is why the
//! accessor lives in `__private` and there is no constructor for a caller to
//! call — or to forget to call.

use linkme::distributed_slice;
use nitinol_persistence::__private::linked_event_registry;
use nitinol_persistence::{EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY};

const MARKER: EventType =
    EventType::new(Family::new("persistence.linked"), TypeName::new("Marker"));
const SOLO: EventType = EventType::new(Family::new(""), TypeName::new("Solo"));

#[distributed_slice(EVENT_REGISTRY)]
static MARKER_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry_linked::Marker", &[MARKER]);

#[distributed_slice(EVENT_REGISTRY)]
static SOLO_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new("event_registry_linked::Solo", &[SOLO]);

#[test]
fn an_entry_placed_in_the_slice_is_collected_with_no_registration_by_hand() {
    let registry = linked_event_registry()
        .expect("a binary whose entries claim distinct canonical paths builds its registry");

    assert_eq!(
        registry.resolve("persistence.linked.Marker"),
        Some(MARKER),
        "a declaration is collected under the one canonical path it is written down as"
    );
    assert_eq!(
        registry.resolve("Solo"),
        Some(SOLO),
        "a second declaration is collected alongside the first, not instead of it"
    );
}

#[test]
fn a_path_no_linked_entry_declares_resolves_to_nothing() {
    let registry = linked_event_registry()
        .expect("a binary whose entries claim distinct canonical paths builds its registry");

    assert_eq!(
        registry.resolve("persistence.linked.Absent"),
        None,
        "collection reaches what the linker found and nothing else: a type no linked entry \
         declares stays unknown, which is what a row carrying it fails its load against"
    );
    assert_eq!(
        registry.resolve("persistence.linked.Marker.Raised"),
        None,
        "a declaration answers for its own path and for no path beneath it: a row carrying \
         anything deeper names a type nobody declared"
    );
}

#[test]
fn the_registry_handed_out_is_the_one_the_process_already_built() {
    let first =
        linked_event_registry().expect("a binary whose entries claim distinct paths builds one");
    let second =
        linked_event_registry().expect("a binary whose entries claim distinct paths builds one");

    assert!(
        std::ptr::eq(first, second),
        "the registry is built once for the process and handed out again: a caller that got a \
         registry of its own would be paying to read the slice for every store it opens, and two \
         registries of one binary could not disagree about anything except by one of them being \
         stale"
    );
}
