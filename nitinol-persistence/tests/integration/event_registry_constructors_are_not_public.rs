//! `EventRegistry::from_linked` and `EventRegistry::from_entries` must be gone
//! from the crate's public API.
//!
//! ```gherkin
//!   Scenario: a downstream definition claims `from_linked`
//!     Given a trait of this test's own that states `EventRegistry::from_linked`
//!     When the bare associated function is called
//!     Then it denotes the downstream definition
//!
//!   Scenario: a downstream definition claims `from_entries`
//!     Then it denotes the downstream definition the same way
//! ```
//!
//! A name that no longer exists cannot be imported, so the contract is observed
//! by letting a downstream definition claim it. Both are associated functions
//! rather than methods, and an inherent associated function wins name resolution
//! over a trait one:
//!
//! - Once the crate stops declaring them, the bare names have a single source
//!   and resolve to the extension trait below.
//! - While the crate still declares them, the inherent ones are selected and
//!   this test binary fails to build — `from_linked` returns a
//!   `Result<EventRegistry, _>` where a `&str` is stated, and `from_entries`
//!   takes the entries this call does not pass.
//!
//! What replaced them is judged where it is used: `event_registry_linked.rs`
//! for the process-wide registry, `event_registry.rs` for building one out of
//! entries handed over by hand.

use nitinol_persistence::EventRegistry;

const PROBE: &str = "downstream-probe";

/// What a downstream crate is free to state about `EventRegistry` once the two
/// names are released.
trait DownstreamConstructors {
    fn from_linked() -> &'static str;
    fn from_entries() -> &'static str;
}

impl DownstreamConstructors for EventRegistry {
    fn from_linked() -> &'static str {
        PROBE
    }

    fn from_entries() -> &'static str {
        PROBE
    }
}

/// Given the extension trait is in scope, when the bare `from_linked` is called,
/// then it denotes the downstream definition.
#[test]
fn from_linked_is_a_name_a_downstream_definition_may_claim() {
    assert_eq!(
        EventRegistry::from_linked(),
        PROBE,
        "building the process-wide registry is the framework's own business now, so the crate must \
         not hold `from_linked` in its public API"
    );
}

/// Given the extension trait is in scope, when the bare `from_entries` is
/// called, then it denotes the downstream definition.
#[test]
fn from_entries_is_a_name_a_downstream_definition_may_claim() {
    assert_eq!(
        EventRegistry::from_entries(),
        PROBE,
        "handing entries over by hand is the defence layer's own business now, so the crate must \
         not hold `from_entries` in its public API"
    );
}
