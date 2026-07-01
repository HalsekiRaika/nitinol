// Both Receive traits must coexist in the same scope without name conflict.
// Users disambiguate by aliasing the runtime version (or vice versa).
use nitinol_eventsource::Receive;
use nitinol_runtime::process::Receive as RuntimeReceive;

use async_trait::async_trait;
use nitinol_eventsource::{Aggregate, Context, Event};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_runtime::process::{Process, ProcessContext};

// ---------------------------------------------------------------------------
// Fixtures for nitinol_eventsource::Receive
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Counted"));
}

#[derive(Default)]
struct ReadCounter {
    value: u64,
}

impl Aggregate for ReadCounter {
    type Event = Counted;

    fn apply(&mut self, _event: Counted) {
        self.value += 1;
    }
}

struct GetValue;

#[async_trait]
impl Receive<GetValue> for ReadCounter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(
        &self,
        _msg: GetValue,
        _ctx: &mut Context,
    ) -> Result<u64, std::convert::Infallible> {
        Ok(self.value)
    }
}

// ---------------------------------------------------------------------------
// Fixtures for nitinol_runtime::process::Receive (RuntimeReceive)
// Used to demonstrate coexistence — both traits can be implemented in the
// same file without conflicting.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct DummyProcess;
#[allow(dead_code)]
struct DummyMsg;

impl Process for DummyProcess {}

impl RuntimeReceive<DummyMsg> for DummyProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: DummyMsg,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Receive<GetValue>: response from state
// ---------------------------------------------------------------------------

/// recv(GetValue) returns the current counter value without mutating state
#[tokio::test]
async fn recv_returns_computed_value_from_state() {
    // Given
    let mut counter = ReadCounter::default();
    counter.apply(Counted);
    counter.apply(Counted);
    let mut ctx = Context::new(AggregateId::new("read-counter"), 2);

    // When
    let value = counter
        .recv(GetValue, &mut ctx)
        .await
        .expect("recv must not fail");

    // Then
    assert_eq!(value, 2, "recv must return the current counter value (2)");
}

/// recv(GetValue) on default counter (value 0) returns 0
#[tokio::test]
async fn recv_with_zero_state_returns_zero() {
    // Given
    let counter = ReadCounter::default();
    let mut ctx = Context::new(AggregateId::new("read-counter"), 0);

    // When
    let value = counter
        .recv(GetValue, &mut ctx)
        .await
        .expect("recv must not fail for zero state");

    // Then
    assert_eq!(value, 0, "recv must return 0 for default state");
}

// ---------------------------------------------------------------------------
// &self immutability: recv must not change aggregate state
// ---------------------------------------------------------------------------

/// Calling recv(GetValue) does not mutate ReadCounter.value
/// — enforced by &self in the trait signature; verified here at runtime.
#[tokio::test]
async fn recv_does_not_mutate_state_via_self_ref() {
    // Given
    let mut counter = ReadCounter::default();
    counter.apply(Counted);
    counter.apply(Counted);
    counter.apply(Counted);
    let mut ctx = Context::new(AggregateId::new("read-counter"), 3);

    // When
    let _value = counter
        .recv(GetValue, &mut ctx)
        .await
        .expect("recv must succeed");

    // Then: value unchanged because recv takes &self
    assert_eq!(
        counter.value, 3,
        "ReadCounter.value must remain 3 — recv(&self) must not mutate state"
    );
}

// ---------------------------------------------------------------------------
// Coexistence: both Receive traits compile in the same scope
// ---------------------------------------------------------------------------

/// nitinol_eventsource::Receive and nitinol_runtime::process::Receive can be
/// imported simultaneously by aliasing one.
/// This test proves that both impls are callable without ambiguity.
#[tokio::test]
async fn both_receive_traits_coexist_in_same_scope() {
    // Given: ReadCounter implements nitinol_eventsource::Receive (bound as `Receive`)
    let counter = ReadCounter::default();
    let mut ctx = Context::new(AggregateId::new("coexistence-test"), 0);

    // When: call eventsource::Receive::recv — no ambiguity because `Receive` is the
    // eventsource version and `RuntimeReceive` is the runtime alias.
    let value: u64 = counter
        .recv(GetValue, &mut ctx)
        .await
        .expect("eventsource recv must succeed");

    // Then
    assert_eq!(value, 0, "eventsource recv must return state-derived value");
    // The fact that this file compiles with both use statements in scope is
    // the primary assertion for coexistence.
}
