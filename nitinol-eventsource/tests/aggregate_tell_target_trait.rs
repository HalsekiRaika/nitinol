#![cfg(feature = "test-helpers")]

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{Aggregate, AggregateProxy, AggregateTellTarget, Event};
use nitinol_persistence::EventType;

#[derive(Clone, Debug)]
struct Noop;

impl Event for Noop {
    const EVENT_TYPE: EventType = EventType::from_str("tell_target.Noop");
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
