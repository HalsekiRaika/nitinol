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
//! ```
//!
//! The entries below stand for what `#[derive(Event)]` writes: one entry per
//! declaring type, carrying the one canonical path that type is written down
//! as. A hand-written `impl Event` places the same shape itself, which is why
//! the entry type and the slice are public rather than hidden behind the derive.

use linkme::distributed_slice;
use nitinol_persistence::{
    EventRegistry, EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY,
};

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
    let registry = EventRegistry::from_linked()
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
    let registry = EventRegistry::from_linked()
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
