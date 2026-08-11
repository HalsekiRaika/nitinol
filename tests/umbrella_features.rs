// Feature: runtime

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

// Feature: persistence

/// When the "persistence" feature is enabled, nitinol::persistence types must
/// be accessible.
#[cfg(feature = "persistence")]
#[test]
fn persistence_feature_exposes_id_types() {
    use nitinol::persistence::{AggregateId, ProjectionId};

    let _agg = AggregateId::new("test");
    let _proj = ProjectionId::new("test");
}

// Feature: eventsource

/// When the "eventsource" feature is enabled, nitinol::eventsource types must
/// be accessible, including the aggregate and projection APIs.
#[cfg(feature = "eventsource")]
#[test]
fn eventsource_feature_exposes_aggregate_and_projection() {
    // These types come from nitinol::eventsource re-export
    use nitinol::eventsource::{Aggregate, AggregateProps, Event, ProjectorProps};
    // EventType lives in nitinol-persistence; the eventsource feature transitively
    // enables the persistence feature, so nitinol::persistence is re-exported.
    use nitinol::persistence::{EventType, Family, TypeName};

    // Minimal dummy types satisfying the trait bounds required by AggregateProps<A: Aggregate>.
    #[derive(Clone)]
    struct DummyEvent;
    impl Event for DummyEvent {
        const EVENT_TYPE: EventType = EventType::new(Family::new("dummy"), TypeName::new("event"));
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

// Feature: eventsource — SystemEvent system must NOT be exposed at umbrella root

/// Regression guard: the umbrella facade (`pub mod eventsource`) must expose all
/// user-facing root items and sub-modules, while deliberately NOT including
/// `SystemEvent`, `appending_system_event`, or `SystemEventDecodeError`
/// (excluded from the umbrella re-export).
///
/// The test body is a compile-time check only: it passes if every import
/// resolves successfully.  No runtime assertions are needed because the
/// absence of the three framework-internal items is enforced structurally by
/// the facade's explicit re-export list.
#[cfg(feature = "eventsource")]
#[test]
fn eventsource_facade_exposes_user_api_without_system_event() {
    // Root-level re-exports
    #[allow(unused_imports)]
    use nitinol::eventsource::{
        // core traits
        Aggregate,
        // process builder
        AggregateProps,
        AggregateProxy,
        AggregateTellTarget,
        // errors
        AskError,
        CodecSet,
        CodecUnset,
        // context
        Context,
        // durable stream
        CursorSet,
        CursorUnset,
        Decider,
        DurableStream,
        DurableStreamProxy,
        DurableSubscription,
        // effect
        Effect,
        Event,
        // projection
        EventEnvelope,
        EventSet,
        EventUnset,
        ExecError,
        OriginSet,
        OriginUnset,
        ProjectionContext,
        Projector,
        ProjectorProps,
        Receive,
        SequenceCursor,
        SideEffect,
        SideEffectError,
        // snapshot
        SnapshotPersistor,
        SnapshotPersistorProxy,
        Snapshotable,
        TellError,
        TxProvider,
    };
    // Sub-modules must be accessible.
    #[allow(unused_imports)]
    use nitinol::eventsource::{codec, error, projection, system};
    // The test passes if the above imports all compile.
}

// Feature: eventsource — error facade exposes only user-facing types

/// Regression guard: `nitinol::eventsource::error` must expose exactly the
/// five user-facing error types and nothing else.
///
/// `SystemEventDecodeError` (framework-internal) must NOT be importable via
/// `nitinol::eventsource::error`. The negative check is enforced structurally
/// by the explicit re-export list in `src/lib.rs` and by the `compile_fail`
/// doctest on the `eventsource` module.
///
/// This test covers the positive side: all five user-facing types must be
/// accessible through the `error` facade.
#[cfg(feature = "eventsource")]
#[test]
fn eventsource_error_facade_exposes_user_facing_types() {
    #[allow(unused_imports)]
    use nitinol::eventsource::error::{
        AskError, CodecError, EffectExecutionError, ExecError, TellError,
    };

    // CodecError and EffectExecutionError are concrete enums; verify they are in scope.
    let _ = std::marker::PhantomData::<CodecError>;
    let _ = std::marker::PhantomData::<EffectExecutionError>;
    // AskError<R>, ExecError<E> have type parameters — use Infallible as a stand-in.
    let _ = std::marker::PhantomData::<AskError<std::convert::Infallible>>;
    let _ = std::marker::PhantomData::<ExecError<std::convert::Infallible>>;
    let _ = std::marker::PhantomData::<TellError>;
    // If `SystemEventDecodeError` were in the facade the `compile_fail` doctest
    // in src/lib.rs would catch it at `cargo test --doc`.
}

// Default: no features enabled → nothing is re-exported

/// With no features enabled, the umbrella crate must still compile (empty lib.rs).
/// This is the zero-dependency default, matching Tokio's approach.
#[test]
fn umbrella_compiles_with_no_features_enabled() {
    // Nothing to import: this test just verifies the crate builds with no features.
    // The test body is intentionally empty.
}
