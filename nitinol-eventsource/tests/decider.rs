mod common;
use common::{shape_of, Shape};

use async_trait::async_trait;
use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event};
use nitinol_persistence::{AggregateId, EventType};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::from_str("Incremented");
}

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Cmd1 — always succeeds and emits an Incremented event.
struct Increment;

// Cmd2 — fails when value is already at or above the given threshold.
struct IncrementIfLessThan(u64);

#[derive(Debug, thiserror::Error)]
#[error("counter already at maximum: {0}")]
struct AtMaxError(u64);

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Self::Event>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[async_trait]
impl Decider<IncrementIfLessThan> for Counter {
    type Rejection = AtMaxError;

    async fn decide(
        &self,
        cmd: IncrementIfLessThan,
        _ctx: &mut Context,
    ) -> Result<Effect<Self::Event>, Self::Rejection> {
        if self.value >= cmd.0 {
            Err(AtMaxError(self.value))
        } else {
            Ok(Effect::persist(Incremented))
        }
    }
}

// ---------------------------------------------------------------------------
// Decider<Increment>: success path
// ---------------------------------------------------------------------------

/// decide(Increment) returns Effect::Persist([Incremented])
#[tokio::test]
async fn decide_increment_returns_persist_incremented() {
    // Given
    let counter = Counter::default();
    let mut ctx = Context::new(AggregateId::new("test-counter"), 0);

    // When
    let effect = counter
        .decide(Increment, &mut ctx)
        .await
        .expect("decide(Increment) must not fail");

    // Then
    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![Incremented]),
        "decide(Increment) must return Persist([Incremented])"
    );
}

// ---------------------------------------------------------------------------
// Decider<IncrementIfLessThan>: success path
// ---------------------------------------------------------------------------

/// decide(IncrementIfLessThan(1)) when value=0 returns Effect::Persist([Incremented])
#[tokio::test]
async fn decide_increment_if_less_than_success() {
    // Given
    let counter = Counter { value: 0 };
    let mut ctx = Context::new(AggregateId::new("test-counter"), 0);

    // When
    let effect = counter
        .decide(IncrementIfLessThan(1), &mut ctx)
        .await
        .expect("decide should succeed when value < max");

    // Then
    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![Incremented]),
        "decide should return Persist([Incremented]) when below threshold"
    );
}

// ---------------------------------------------------------------------------
// Decider<IncrementIfLessThan>: rejection path
// ---------------------------------------------------------------------------

/// decide(IncrementIfLessThan(0)) when value=0 returns AtMaxError
#[tokio::test]
async fn decide_increment_if_less_than_rejection() {
    // Given
    let counter = Counter { value: 0 };
    let mut ctx = Context::new(AggregateId::new("test-counter"), 0);

    // When
    let result = counter.decide(IncrementIfLessThan(0), &mut ctx).await;

    // Then
    assert!(result.is_err(), "decide must fail when value >= threshold");
    // Use .err().unwrap() instead of .unwrap_err() because Effect<E> does not
    // implement Debug (required by unwrap_err's T: Debug bound).
    let err = result.err().unwrap();
    assert_eq!(err.0, 0, "AtMaxError must carry the current value");
}

/// decide(IncrementIfLessThan(5)) when value=5 returns AtMaxError(5)
#[tokio::test]
async fn decide_increment_if_less_than_rejection_at_boundary() {
    // Given
    let counter = Counter { value: 5 };
    let mut ctx = Context::new(AggregateId::new("test-counter"), 4);

    // When
    let result = counter.decide(IncrementIfLessThan(5), &mut ctx).await;

    // Then
    assert!(
        result.is_err(),
        "decide must fail at exact boundary (value == threshold)"
    );
    // Use .err().unwrap() instead of .unwrap_err() because Effect<E> does not
    // implement Debug (required by unwrap_err's T: Debug bound).
    let err = result.err().unwrap();
    assert_eq!(err.0, 5, "AtMaxError must carry value 5");
}

// ---------------------------------------------------------------------------
// &self immutability: decide must not change aggregate state
// ---------------------------------------------------------------------------

/// Calling decide(Increment) does not mutate counter.value
/// — enforced by &self in the trait signature; verified here at runtime.
#[tokio::test]
async fn decide_does_not_mutate_state_via_self_ref() {
    // Given
    let counter = Counter { value: 42 };
    let mut ctx = Context::new(AggregateId::new("test-counter"), 10);

    // When
    let _effect = counter
        .decide(Increment, &mut ctx)
        .await
        .expect("decide must succeed");

    // Then: state is unchanged because decide takes &self
    assert_eq!(
        counter.value, 42,
        "counter.value must remain 42 — decide(&self) must not mutate state"
    );
}
