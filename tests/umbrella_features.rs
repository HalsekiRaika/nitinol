// ---------------------------------------------------------------------------
// Feature: runtime
// ---------------------------------------------------------------------------

/// When the "runtime" feature is enabled, nitinol::runtime re-exports must be
/// accessible and the ProcessSystem must be constructible.
#[cfg(feature = "runtime")]
#[tokio::test]
async fn runtime_feature_exposes_process_system() {
    // Given: import via the umbrella crate under the "runtime" feature
    // (import statement is in the test body to avoid compile errors without the feature)
    use nitinol::runtime::ProcessSystem;

    // When: create a ProcessSystem via the umbrella re-export
    let _system = ProcessSystem::new().await;

    // Then: no panic, system is usable
}

// ---------------------------------------------------------------------------
// Feature: persistence
// ---------------------------------------------------------------------------

/// When the "persistence" feature is enabled, nitinol::persistence types must
/// be accessible.
#[cfg(feature = "persistence")]
#[test]
fn persistence_feature_exposes_id_types() {
    use nitinol::persistence::{AggregateId, ProjectionId};

    let _agg = AggregateId::new("test");
    let _proj = ProjectionId::new("test");
}

// ---------------------------------------------------------------------------
// Feature: eventsource
// ---------------------------------------------------------------------------

/// When the "eventsource" feature is enabled, nitinol::eventsource types must
/// be accessible, including the aggregate and projection APIs.
#[cfg(feature = "eventsource")]
#[test]
fn eventsource_feature_exposes_aggregate_and_projection() {
    // These types come from nitinol::eventsource re-export
    use nitinol::eventsource::{Aggregate, AggregateProps, Event, ProjectorProps};
    // EventType lives in nitinol-persistence; the eventsource feature transitively
    // enables the persistence feature, so nitinol::persistence is re-exported.
    use nitinol::persistence::EventType;

    // Minimal dummy types satisfying the trait bounds required by AggregateProps<A: Aggregate>.
    #[derive(Clone)]
    struct DummyEvent;
    impl Event for DummyEvent {
        const EVENT_TYPE: EventType = EventType::from_str("dummy.event");
    }

    #[derive(Default)]
    struct DummyAggregate;
    impl Aggregate for DummyAggregate {
        type Event = DummyEvent;
        fn apply(&mut self, _event: DummyEvent) {}
    }

    // Compile-time check: the types are in scope and bounds are satisfied.
    let _ = std::marker::PhantomData::<AggregateProps<DummyAggregate>>;
    let _ = std::marker::PhantomData::<ProjectorProps<(), ()>>;
}

// ---------------------------------------------------------------------------
// Default: no features enabled → nothing is re-exported
// ---------------------------------------------------------------------------

/// With no features enabled, the umbrella crate must still compile (empty lib.rs).
/// This is the zero-dependency default, matching Tokio's approach.
#[test]
fn umbrella_compiles_with_no_features_enabled() {
    // Nothing to import: this test just verifies the crate builds with no features.
    // The test body is intentionally empty.
}
