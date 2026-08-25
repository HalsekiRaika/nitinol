#![cfg(feature = "test-helpers")]

use std::marker::PhantomData;

use futures_core::future::BoxFuture;
use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{Aggregate, AggregateProxy, AggregateTellTarget, Decider, Event};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};

#[derive(Clone, Debug)]
struct Noop;

impl Event for Noop {
    const EVENT_TYPE: EventType = EventType::new(Family::new("tell_target"), TypeName::new("Noop"));
}

#[derive(Default)]
struct Target;

impl Aggregate for Target {
    type Event = Noop;

    fn apply(&mut self, _event: Noop) {}
}

fn assert_aggregate_tell_target<A: Aggregate, T: AggregateTellTarget<A>>() {}

#[test]
fn aggregate_proxy_implements_aggregate_tell_target() {
    assert_aggregate_tell_target::<Target, AggregateProxy<Target>>();
}

#[test]
fn mock_aggregate_proxy_implements_aggregate_tell_target() {
    assert_aggregate_tell_target::<Target, MockAggregateProxy<Target>>();
}

// contract-wiring: the id accessor must be provided by every implementor.
//
// This test exists to prevent regression: if a future change re-introduces a
// default implementation of `aggregate_id`, the test below would still
// compile but the behavioral guard would be lost.  The primary protection is
// that `aggregate_id` has **no default** in the trait definition — any
// implementation that omits it fails to compile.

/// The stream key [`MinimalTarget`] owns and is expected to hand back verbatim.
const MINIMAL_TARGET_STREAM_KEY: &str = "minimal-target-stream-key";

/// A minimal implementation that explicitly provides `aggregate_id`.
/// If `aggregate_id` is re-introduced as a default in the trait, this test
/// still compiles but the type-boundary enforcement is gone.
struct MinimalTarget<A> {
    aggregate_id: AggregateId,
    _phantom: PhantomData<fn() -> A>,
}

impl<A> Clone for MinimalTarget<A> {
    fn clone(&self) -> Self {
        Self {
            aggregate_id: self.aggregate_id.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for MinimalTarget<A> {
    fn tell<C>(&'_ self, _cmd: C) -> BoxFuture<'_, Result<(), nitinol_eventsource::TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async { Ok(()) })
    }

    /// Required — no default exists in the trait.
    fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }
}

/// contract-wiring: the trait accessor hands back the implementor's own typed
/// id.
///
/// The binding to `&AggregateId` is part of the assertion: a raw-string
/// accessor would not type-check here, so this pins the boundary at a typed id
/// rather than a `&str` downgrade.
#[test]
fn aggregate_id_returns_the_implementors_typed_id() {
    let target: MinimalTarget<Target> = MinimalTarget {
        aggregate_id: AggregateId::new(MINIMAL_TARGET_STREAM_KEY),
        _phantom: PhantomData,
    };

    let id: &AggregateId =
        <MinimalTarget<Target> as AggregateTellTarget<Target>>::aggregate_id(&target);

    assert_eq!(
        id,
        &AggregateId::new(MINIMAL_TARGET_STREAM_KEY),
        "AggregateTellTarget::aggregate_id() must hand back the id the implementor owns"
    );
}

/// contract-wiring: `MockAggregateProxy::aggregate_id` is non-empty
/// so it is safe to use in TellIntent-based tests.
#[test]
fn mock_aggregate_proxy_aggregate_id_is_non_empty() {
    let proxy: MockAggregateProxy<Target> = MockAggregateProxy::new();
    let id: &AggregateId =
        <MockAggregateProxy<Target> as AggregateTellTarget<Target>>::aggregate_id(&proxy);
    assert!(
        !id.as_str().is_empty(),
        "MockAggregateProxy must return a non-empty aggregate_id for safe TellIntent use"
    );
}
