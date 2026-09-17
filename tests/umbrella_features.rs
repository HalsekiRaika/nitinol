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
    use nitinol::contract::Codec;
    use nitinol::eventsource::{Aggregate, AggregateProps, Event, ProjectorProps};
    // EventType lives in nitinol-persistence; the eventsource feature transitively
    // enables the persistence feature, so nitinol::persistence is re-exported.
    use nitinol::persistence::{EventType, Family, TypeName};

    // Minimal dummy types satisfying the trait bounds required by AggregateProps<A: Aggregate>.
    #[derive(Clone)]
    struct DummyEvent;

    struct DummyCodec;
    impl Codec<DummyEvent> for DummyCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &DummyEvent) -> Result<bytes::Bytes, Self::Error> {
            Ok(bytes::Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<DummyEvent, Self::Error> {
            Ok(DummyEvent)
        }
    }

    impl Event for DummyEvent {
        const EVENT_TYPE: EventType = EventType::new(Family::new("dummy"), TypeName::new("event"));

        type Codec = DummyCodec;
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
        // decision
        Accepting,
        // core traits
        Aggregate,
        // process builder
        AggregateProps,
        AggregateProxy,
        AggregateTellTarget,
        // errors
        AskError,
        // durable stream
        CursorSet,
        CursorUnset,
        Decider,
        Decision,
        DurableStream,
        DurableStreamProxy,
        DurableSubscription,
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
        // query
        Query,
        SequenceCursor,
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
/// user-facing error types and nothing else.
///
/// `SystemEventDecodeError` (framework-internal) must NOT be importable via
/// `nitinol::eventsource::error`. The negative check is enforced structurally
/// by the explicit re-export list in `src/lib.rs` and by the `compile_fail`
/// doctest on the `eventsource` module.
///
/// This test covers the positive side: every user-facing type must be
/// accessible through the `error` facade.
#[cfg(feature = "eventsource")]
#[test]
fn eventsource_error_facade_exposes_user_facing_types() {
    #[allow(unused_imports)]
    use nitinol::eventsource::error::{AskError, CodecError, ExecError, PersistError, TellError};

    // CodecError and PersistError are concrete enums; verify they are in scope.
    let _ = std::marker::PhantomData::<CodecError>;
    let _ = std::marker::PhantomData::<PersistError>;
    // AskError<R>, ExecError<E> have type parameters — use Infallible as a stand-in.
    let _ = std::marker::PhantomData::<AskError<std::convert::Infallible>>;
    let _ = std::marker::PhantomData::<ExecError<std::convert::Infallible>>;
    let _ = std::marker::PhantomData::<TellError>;
    // If `SystemEventDecodeError` were in the facade the `compile_fail` doctest
    // in src/lib.rs would catch it at `cargo test --doc`.
}

// Feature: postgres

/// When the "postgres" feature is enabled, the sqlx provider must be reachable
/// through the umbrella crate, and its store must be usable as the framework's
/// `Arc<dyn EventStore>` handle.
///
/// Without the feature the provider is not part of the build at all, which is
/// what `optional = true` on the dependency gives: there is nothing to import
/// and this test is not compiled.
#[cfg(feature = "postgres")]
#[test]
fn postgres_feature_exposes_the_sqlx_event_store() {
    use std::sync::Arc;

    use nitinol::persistence::store::EventStore;
    use nitinol::persistence::{EventRegistry, EventRegistryError};
    use nitinol::sqlx::sqlx::PgPool;
    use nitinol::sqlx::PostgresEventStore;

    // Compile-time check: what the provider builds is what the framework plugs in.
    fn plugged_in(store: PostgresEventStore) -> Arc<dyn EventStore> {
        Arc::new(store)
    }
    let _: fn(PostgresEventStore) -> Arc<dyn EventStore> = plugged_in;

    // A store is opened over a pool alone: the event types it serves are the
    // ones its binary linked, so there is no registry for a caller to pass in
    // — and no registry for a caller to forget to pass in, which is what left
    // a saga's own records unrestorable.
    let _: fn(PgPool) -> Result<PostgresEventStore, EventRegistryError> = PostgresEventStore::new;

    let registry = EventRegistry::from_linked()
        .expect("a build whose linked event types claim distinct canonical paths builds one");
    assert!(
        registry.resolve("dummy.event").is_none(),
        "collection answers for the event types the build linked, and nothing declares this one"
    );
}

// Default: no features enabled → nothing is re-exported

/// With no features enabled, the umbrella crate must still compile (empty lib.rs).
/// This is the zero-dependency default, matching Tokio's approach.
#[test]
fn umbrella_compiles_with_no_features_enabled() {
    // Nothing to import: this test just verifies the crate builds with no features.
    // The test body is intentionally empty.
}
