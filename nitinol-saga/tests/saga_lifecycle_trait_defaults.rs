//! Lifecycle hook defaults: the `Saga` trait carries working default
//! implementations for its optional hooks, so a user can implement `Saga`
//! without overriding any of them.
//!
//! - `on_scheduled(..)` defaults to a no-op that returns `SagaEffect::None`.
//! - `on_tell_failed(..)` defaults to a no-op that returns `SagaEffect::None`.

#[path = "common/helpers.rs"]
mod common;
use common::{shape_of, Shape};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

// A saga that overrides ONLY the required methods, leaving on_scheduled and
// on_tell_failed at their trait defaults.

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StubEvent;

impl Event for StubEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("trait_defaults"), TypeName::new("StubEvent"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StubUpstream;

impl Event for StubUpstream {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("trait_defaults"), TypeName::new("StubUpstream"));
}

/// Correlation rule of [`StubSaga`]: the one instance the hooks below are
/// exercised against.  `correlate` has no default body, so even a saga that is
/// never spawned — these tests call its hooks directly — must declare one.
const STUB_SAGA_ID: &str = "trait-defaults-stub";

#[derive(Default)]
struct StubSaga;

#[async_trait]
impl Saga for StubSaga {
    type SubscribedEvent = StubUpstream;
    type Event = StubEvent;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(STUB_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// The default `on_scheduled` implementation must be a no-op that yields
/// `SagaEffect::None` — a saga that does not override the hook opts out of
/// scheduled handling and produces no effect when a timer fires.
#[tokio::test]
async fn default_on_scheduled_returns_none_effect() {
    let mut saga = StubSaga;
    let mut ctx = SagaContext::test_context(SagaId::new("on-scheduled-default"), 0);

    let effect = saga
        .on_scheduled((), &mut ctx)
        .await
        .expect("the default on_scheduled must return Ok");

    assert_eq!(
        shape_of(&effect),
        Shape::None,
        "the default on_scheduled must produce SagaEffect::None"
    );
}

/// The default `on_tell_failed` implementation must be a no-op that yields
/// `SagaEffect::None`.  `StubSaga` overrides only the required methods, so this
/// test also pins that a saga written before the hook existed still compiles
/// and still reacts to a tell failure with no effect at all.
#[tokio::test]
async fn default_on_tell_failed_returns_none_effect() {
    let mut saga = StubSaga;
    let mut ctx = SagaContext::test_context(SagaId::new("on-tell-failed-default"), 0);

    let effect = saga
        .on_tell_failed(AggregateId::new("unreachable-target"), &mut ctx)
        .await
        .expect("the default on_tell_failed must return Ok");

    assert_eq!(
        shape_of(&effect),
        Shape::None,
        "the default on_tell_failed must produce SagaEffect::None"
    );
}
