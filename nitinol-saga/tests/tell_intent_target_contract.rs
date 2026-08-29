//! `TellIntent`'s tell-target contract: the `AggregateId` a tell target
//! reports must be non-empty.
//!
//! The constraint cannot live in `AggregateId` itself, because an empty
//! `AggregateId` is a legitimate framework value — `SourceContext`'s
//! "no upstream aggregate" sentinel is exactly that.  Being non-empty is
//! specific to the *tell target* role, so `TellIntent` construction is the
//! boundary that owns and enforces it.
//!
//! Both constructors (`new` and `new_with_crash_restart`) build the same
//! `target_id`, so both are pinned here: a guard applied to only one of them
//! would let the other through.

use bytes::Bytes;
use futures_core::future::BoxFuture;

use nitinol_eventsource::{Aggregate, AggregateTellTarget, Decider, Decision, Event, TellError};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::TellIntent;

#[derive(Clone, Debug)]
struct Noop;

impl Event for Noop {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("tell_intent_target"), TypeName::new("Noop"));
}

#[derive(Default)]
struct TargetAggregate;

impl Aggregate for TargetAggregate {
    type Event = Noop;

    fn apply(&mut self, _event: Noop) {}
}

#[derive(Clone)]
struct DoNothing;

impl Decider<DoNothing> for TargetAggregate {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: DoNothing) -> Decision<Noop, (), Self::Rejection> {
        Decision::persist(vec![Noop]).output(())
    }
}

/// A tell target whose reported id is supplied by the test, so the accepted
/// and the rejected case run through the very same implementation.
#[derive(Clone)]
struct StaticTellTarget {
    aggregate_id: AggregateId,
}

impl StaticTellTarget {
    fn reporting(stream_key: &str) -> Self {
        Self {
            aggregate_id: AggregateId::new(stream_key),
        }
    }
}

impl AggregateTellTarget<TargetAggregate> for StaticTellTarget {
    fn tell<C>(&'_ self, _cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        TargetAggregate: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(async { Ok(()) })
    }

    fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }
}

const TARGET_STREAM_KEY: &str = "tell-intent-contract-target";

fn crash_restart_payload() -> Bytes {
    Bytes::from_static(b"crash-restart")
}

/// Given a target reporting a non-empty `AggregateId`,
/// When `TellIntent::new` is called,
/// Then construction completes — the accepted side of the contract.
#[test]
fn new_accepts_a_target_reporting_a_non_empty_aggregate_id() {
    let target = StaticTellTarget::reporting(TARGET_STREAM_KEY);

    let _intent = TellIntent::new(target, DoNothing);
}

/// The accepted side for the crash-restart constructor.
#[test]
fn new_with_crash_restart_accepts_a_target_reporting_a_non_empty_aggregate_id() {
    let target = StaticTellTarget::reporting(TARGET_STREAM_KEY);

    let _intent = TellIntent::new_with_crash_restart(target, DoNothing, crash_restart_payload());
}

/// Given a target reporting an empty `AggregateId`,
/// When `TellIntent::new` is called,
/// Then construction panics rather than recording a target that could never
/// be used for `SagaFailure::TellFailed` reporting.
#[test]
#[should_panic(expected = "empty")]
fn new_rejects_a_target_reporting_an_empty_aggregate_id() {
    let target = StaticTellTarget::reporting("");

    let _intent = TellIntent::new(target, DoNothing);
}

/// The rejected side for the crash-restart constructor.
#[test]
#[should_panic(expected = "empty")]
fn new_with_crash_restart_rejects_a_target_reporting_an_empty_aggregate_id() {
    let target = StaticTellTarget::reporting("");

    let _intent = TellIntent::new_with_crash_restart(target, DoNothing, crash_restart_payload());
}
