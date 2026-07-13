//! Spec B-5b (Snapshot stub) and E-17 (on_scheduled stub): the `Saga` trait
//! gains lifecycle/extension methods with working default implementations so a
//! user can implement `Saga` without overriding any of them.
//!
//! - `snapshot(&self)` defaults to `None` (no snapshot taken).
//! - `on_scheduled(..)` defaults to a no-op that returns `SagaEffect::None`.
//!
//! `from_snapshot` also gains a default (`unimplemented!()` per spec); it is
//! intentionally not exercised here because it is a panic-by-default stub and
//! there is no public way to construct a `SagaSnapshot` to feed it in this MVP.

mod common;
use common::{shape_of, Shape};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, ScheduledMessage};

// ---------------------------------------------------------------------------
// A saga that overrides ONLY the required methods, leaving snapshot /
// from_snapshot / on_scheduled at their trait defaults.
// ---------------------------------------------------------------------------

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

#[derive(Default)]
struct StubSaga;

#[async_trait]
impl Saga for StubSaga {
    type SubscribedEvent = StubUpstream;
    type Event = StubEvent;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

// ---------------------------------------------------------------------------
// B-5b
// ---------------------------------------------------------------------------

/// The default `snapshot` implementation must return `None` — the MVP takes no
/// snapshots, and a user who does not override it opts into that behaviour.
#[tokio::test]
async fn default_snapshot_returns_none() {
    let saga = StubSaga;
    assert!(
        saga.snapshot().is_none(),
        "the default Saga::snapshot implementation must return None"
    );
}

// ---------------------------------------------------------------------------
// E-17
// ---------------------------------------------------------------------------

/// The default `on_scheduled` implementation must be a no-op that yields
/// `SagaEffect::None` — the scheduler is not implemented in this MVP, so a
/// saga that does not override the hook must produce no effect if ever driven.
#[tokio::test]
async fn default_on_scheduled_returns_none_effect() {
    let mut saga = StubSaga;
    let mut ctx = SagaContext::test_context(SagaId::new("on-scheduled-default"), 0);

    let effect = saga
        .on_scheduled(ScheduledMessage::new(), &mut ctx)
        .await
        .expect("the default on_scheduled must return Ok");

    assert_eq!(
        shape_of(&effect),
        Shape::None,
        "the default on_scheduled must produce SagaEffect::None"
    );
}
