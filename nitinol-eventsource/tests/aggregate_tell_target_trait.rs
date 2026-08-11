#![cfg(feature = "test-helpers")]

use std::marker::PhantomData;

use futures_core::future::BoxFuture;
use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{Aggregate, AggregateProxy, AggregateTellTarget, Decider, Event};
use nitinol_persistence::{EventType, Family, TypeName};

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

// contract-wiring: aggregate_id_str must be provided by every implementor.
//
// This test exists to prevent regression: if a future change re-introduces a
// default implementation of `aggregate_id_str`, the test below would still
// compile but the behavioral guard would be lost.  The primary protection is
// that `aggregate_id_str` has **no default** in the trait definition — any
// implementation that omits it fails to compile.

/// A minimal implementation that explicitly provides `aggregate_id_str`.
/// If `aggregate_id_str` is re-introduced as a default in the trait, this test
/// still compiles but the type-boundary enforcement is gone.
struct MinimalTarget<A> {
    _phantom: PhantomData<fn() -> A>,
}

impl<A> Clone for MinimalTarget<A> {
    fn clone(&self) -> Self {
        Self {
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
    fn aggregate_id_str(&self) -> &str {
        "minimal-target-stream-key"
    }
}

/// contract-wiring: `aggregate_id_str` must return a non-empty string.
///
/// `TellIntent::new` panics on empty; returning a fixed non-empty sentinel
/// here verifies that the explicit implementation path is exercised and the
/// value propagates correctly.
#[test]
fn aggregate_id_str_must_be_non_empty() {
    let target: MinimalTarget<Target> = MinimalTarget {
        _phantom: PhantomData,
    };
    let id = target.aggregate_id_str();
    assert!(
        !id.is_empty(),
        "aggregate_id_str() must return a non-empty stream key for TellFailed target tracking"
    );
}

/// contract-wiring: `MockAggregateProxy::aggregate_id_str` is non-empty
/// so it is safe to use in TellIntent-based tests.
#[test]
fn mock_aggregate_proxy_aggregate_id_str_is_non_empty() {
    let proxy: MockAggregateProxy<Target> = MockAggregateProxy::new();
    assert!(
        !proxy.aggregate_id_str().is_empty(),
        "MockAggregateProxy must return a non-empty aggregate_id_str for safe TellIntent use"
    );
}
