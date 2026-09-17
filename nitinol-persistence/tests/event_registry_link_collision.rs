//! A canonical path claimed twice, reached the way a deployment reaches it.
//!
//! ```gherkin
//!   Scenario: canonical path 衝突は構築時に検出される
//!     Given two event definitions colliding on one canonical path
//!     When an EventRegistry is built
//!     Then the build fails with an explicit error
//! ```
//!
//! A binary of its own, because the slice is one per binary: entries that must
//! collide cannot share a binary with a clause that requires a registry to
//! build. `event_registry.rs` judges the same validation with entries handed
//! over by hand; what is judged here is that the linked path runs it at all,
//! rather than collecting entries and trusting them.

use linkme::distributed_slice;
use nitinol_persistence::{
    EventRegistry, EventRegistryEntry, EventType, Family, TypeName, Variant, EVENT_REGISTRY,
};

const STRUCT_C: EventType = EventType::new(Family::new("collision.link"), TypeName::new("C"));
const ARM_C: EventType = EventType::with_variant(
    Family::new("collision"),
    TypeName::new("link"),
    Variant::new("C"),
);

const STRUCT_DECLARER: &str = "event_registry_link_collision::StructC";
const ARM_DECLARER: &str = "event_registry_link_collision::ArmC";

#[distributed_slice(EVENT_REGISTRY)]
static STRUCT_ENTRY: EventRegistryEntry = EventRegistryEntry::new(STRUCT_DECLARER, &[STRUCT_C]);

#[distributed_slice(EVENT_REGISTRY)]
static ARM_ENTRY: EventRegistryEntry = EventRegistryEntry::new(ARM_DECLARER, &[ARM_C]);

#[test]
fn a_collision_among_linked_entries_fails_the_build_of_the_registry() {
    assert_eq!(
        STRUCT_C.to_path().to_string(),
        ARM_C.to_path().to_string(),
        "the two declarations are written down under one canonical path, which is what makes the \
         collision possible"
    );

    let Err(refusal) = EventRegistry::from_linked() else {
        panic!(
            "a binary linking two declarations of one canonical path has no registry that could \
             serve both, and building one was accepted — the rows of whichever lost would be \
             restored as the other"
        )
    };

    let reported = refusal.to_string();
    assert!(
        reported.contains("collision.link.C"),
        "the refusal names the canonical path the linked entries collided on, and it reported \
         {reported:?}"
    );
    assert!(
        reported.contains(STRUCT_DECLARER) && reported.contains(ARM_DECLARER),
        "the refusal names both declarations: collection happens at link time, so the definitions \
         to go and rename are not otherwise in front of the caller. It reported {reported:?}"
    );
}
