//! A canonical path claimed twice, reached the way a deployment reaches it.
//!
//! ```gherkin
//!   Scenario: canonical path 衝突は構築時に検出される
//!     Given two declarations claiming one family and one type name between them
//!     When an EventRegistry is built
//!     Then the build fails with an explicit error
//! ```
//!
//! A binary of its own, because the slice is one per binary: entries that must
//! collide cannot share a binary with a clause that requires a registry to
//! build. `event_registry.rs` judges the same validation with entries handed
//! over by hand; what is judged here is that the linked path runs it at all,
//! rather than collecting entries and trusting them.
//!
//! Two types declaring one path is the collision the grammar still allows. It
//! is not a shape a reader could disentangle: both rows carry the same string,
//! and nothing in the path says which type wrote it.

use linkme::distributed_slice;
use nitinol_persistence::{
    EventRegistry, EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY,
};

const CONTESTED: EventType = EventType::new(Family::new("collision.link"), TypeName::new("C"));

const FIRST_DECLARER: &str = "event_registry_link_collision::FirstC";
const SECOND_DECLARER: &str = "event_registry_link_collision::SecondC";

#[distributed_slice(EVENT_REGISTRY)]
static FIRST_ENTRY: EventRegistryEntry = EventRegistryEntry::new(FIRST_DECLARER, &[CONTESTED]);

#[distributed_slice(EVENT_REGISTRY)]
static SECOND_ENTRY: EventRegistryEntry = EventRegistryEntry::new(SECOND_DECLARER, &[CONTESTED]);

#[test]
fn a_collision_among_linked_entries_fails_the_build_of_the_registry() {
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
        reported.contains(FIRST_DECLARER) && reported.contains(SECOND_DECLARER),
        "the refusal names both declarations: collection happens at link time, so the definitions \
         to go and rename are not otherwise in front of the caller. It reported {reported:?}"
    );
}
