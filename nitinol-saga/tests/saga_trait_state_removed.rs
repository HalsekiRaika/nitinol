//! `Saga` must carry no associated `State` type.
//!
//! The absence of a trait item is not observable at runtime, so the contract is
//! carried by this file building at all: `MinimalSaga` declares every
//! associated type the trait still requires and no `State`.  While
//! `Saga::State` exists the impl is incomplete (E0046) and this test binary
//! fails to build — which is equally true if the item is merely "disabled" by
//! a deprecation attribute or a comment instead of being deleted.
//!
//! What stays observable at runtime is where a saga's state lives: on the
//! implementor itself.  `apply` mutates a field of `MinimalSaga`, and that
//! field is the whole of the saga's state.

use async_trait::async_trait;

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect};

#[derive(Clone)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_state_removed"),
        TypeName::new("Counted"),
    );
}

#[derive(Clone)]
struct Upstream;

impl Event for Upstream {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_state_removed"),
        TypeName::new("Upstream"),
    );
}

/// A saga that implements exactly the required trait items.  Its state is the
/// `applied` field — no associated type participates in holding it.
#[derive(Default)]
struct MinimalSaga {
    applied: u32,
}

#[async_trait]
impl Saga for MinimalSaga {
    type SubscribedEvent = Upstream;
    type Event = Counted;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {
        self.applied += 1;
    }

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Given a saga declared without any associated state type, when its own
/// events are applied, then the state advances on the implementor itself.
#[test]
fn saga_state_lives_on_the_implementor() {
    let mut saga = MinimalSaga::default();

    saga.apply(Counted);
    saga.apply(Counted);

    assert_eq!(
        saga.applied, 2,
        "a saga's state must live on the implementor, not behind an associated type"
    );
}
