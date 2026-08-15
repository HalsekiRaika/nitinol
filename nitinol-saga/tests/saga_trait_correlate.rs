//! `Saga::correlate` — correlation is declared by the saga *type*, not by the
//! spawn wiring.  (Contract `CT-01`.)
//!
//! Two facts are pinned here:
//!
//! 1. **`correlate` is a member of the `Saga` trait** with the signature
//!    `fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId>`.  If the
//!    trait carries no such item, `MinimalCorrelatedSaga`'s `correlate` is not
//!    a trait member (E0407) and this test binary fails to build — which is
//!    equally true of an implementation that keeps the correlation decision
//!    inside `handle` instead of adding the trait item.
//! 2. **It is an associated function, not a method.**  Both tests below call it
//!    as `<MinimalCorrelatedSaga as Saga>::correlate(&event)` and never
//!    construct a `MinimalCorrelatedSaga`.  A `&self` receiver makes that call
//!    fail to compile, and a `SagaId` (non-`Option`) return type makes the
//!    second test fail to compile.
//!
//! `MinimalCorrelatedSaga` implements exactly the trait items the `Saga` trait
//! requires and nothing else, so the required surface stays readable in one
//! place.
//!
//! Not pinned here: that `correlate` carries *no default body*.  The absence of
//! a default is not observable at runtime, and this workspace has no
//! compile-fail harness (`trybuild`) for integration tests, so a default
//! implementation would still let this binary build.  See the test report for
//! the residual check.

use async_trait::async_trait;

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

/// The one correlation rule of [`MinimalCorrelatedSaga`], defined once and
/// consulted by both `correlate` and the assertions below.
const OWNER: &str = "trait-correlate-owner";

/// Upstream event carrying the business key correlation is derived from.
#[derive(Clone)]
struct Signal {
    owner: String,
}

impl Event for Signal {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga_trait_correlate"), TypeName::new("Signal"));
}

/// The saga's own event.
#[derive(Clone)]
struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga_trait_correlate"), TypeName::new("Noted"));
}

/// A saga that implements exactly the required trait items — including
/// `correlate`, which answers "which instance is this event about?" from the
/// event alone.
struct MinimalCorrelatedSaga;

#[async_trait]
impl Saga for MinimalCorrelatedSaga {
    type SubscribedEvent = Signal;
    type Event = Noted;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        if event.owner == OWNER {
            Some(SagaId::new(OWNER))
        } else {
            None
        }
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

/// Given an upstream event whose business key names a saga instance,
/// when `correlate` is invoked through the type with no instance in existence,
/// then it yields that instance's [`SagaId`].
#[test]
fn correlate_names_the_owning_saga_without_an_instance() {
    let event = Signal {
        owner: OWNER.to_owned(),
    };

    assert_eq!(
        <MinimalCorrelatedSaga as Saga>::correlate(&event),
        Some(SagaId::new(OWNER)),
        "correlate must be reachable as an associated function on the saga \
         type — no saga instance exists at the point the runtime has to decide \
         whom the event belongs to"
    );
}

/// Given an upstream event that belongs to no instance of this saga,
/// when `correlate` is invoked,
/// then it declines with `None` rather than naming an instance.
#[test]
fn correlate_declines_an_event_that_belongs_to_no_instance() {
    let event = Signal {
        owner: "some-other-process".to_owned(),
    };

    assert_eq!(
        <MinimalCorrelatedSaga as Saga>::correlate(&event),
        None,
        "correlate must be able to decline an event; a bare `SagaId` return \
         type would force every upstream event to name some instance"
    );
}
