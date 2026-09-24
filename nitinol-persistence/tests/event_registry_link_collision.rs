//! A canonical path claimed twice, reached the way a deployment reaches it.
//!
//! ```gherkin
//!   Scenario: a canonical path collision is caught when the registry is built
//!     Given two declarations claiming one family and one type name between them
//!     When the process-wide EventRegistry is asked for
//!     Then it refuses with an explicit error
//!
//!   Scenario: the refusal a second caller is given
//!     When the registry is asked for again
//!     Then the same refusal is reported — a collision is not consumed by
//!          whoever asked first
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
//!
//! # Why the run-time layer is still here
//!
//! A canonical path claimed twice is refused before the program runs: a
//! declaration carries a symbol named after its path, so the second one is a
//! symbol already defined — rustc's error within a crate, the linker's
//! duplicate-symbol error across crates. That net has holes (static libraries
//! linked together need not report a duplicate at all), and the entries below
//! are placed by hand rather than derived, so they carry no such symbol and
//! reach a running process exactly as a declaration that slipped through would.
//! This is the layer that refuses them there.

use linkme::distributed_slice;
use nitinol_persistence::__private::linked_event_registry;
use nitinol_persistence::{EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY};

const CONTESTED: EventType = EventType::new(Family::new("collision.link"), TypeName::new("C"));

const FIRST_DECLARER: &str = "event_registry_link_collision::FirstC";
const SECOND_DECLARER: &str = "event_registry_link_collision::SecondC";

#[distributed_slice(EVENT_REGISTRY)]
static FIRST_ENTRY: EventRegistryEntry = EventRegistryEntry::new(FIRST_DECLARER, &[CONTESTED]);

#[distributed_slice(EVENT_REGISTRY)]
static SECOND_ENTRY: EventRegistryEntry = EventRegistryEntry::new(SECOND_DECLARER, &[CONTESTED]);

#[test]
fn a_collision_among_linked_entries_fails_the_build_of_the_registry() {
    let Err(refusal) = linked_event_registry() else {
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

#[test]
fn every_caller_is_told_about_the_collision_not_only_the_first() {
    let first = linked_event_registry()
        .expect_err("a binary linking two declarations of one canonical path has no registry")
        .to_string();
    let second = linked_event_registry()
        .expect_err("a binary linking two declarations of one canonical path has no registry")
        .to_string();

    assert_eq!(
        first, second,
        "the registry is built once for the process, and a build that failed stays failed: a \
         refusal handed to whoever asked first and withheld from the next caller would let the \
         second store open over a registry nobody could serve"
    );
}
