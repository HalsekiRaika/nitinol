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

use bytes::Bytes;
use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};

/// The format the events below name.  Nothing in this file writes to a store —
/// `apply` is called directly — but `Event` names the codec that would write
/// it, so one is declared.
struct UnitCodec;

impl Codec<Counted> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Counted) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Counted, Self::Error> {
        Ok(Counted)
    }
}

impl Codec<Upstream> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Upstream) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Upstream, Self::Error> {
        Ok(Upstream)
    }
}

#[derive(Clone)]
struct Counted;

impl Event for Counted {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_state_removed"),
        TypeName::new("Counted"),
    );
    type Codec = UnitCodec;
}

#[derive(Clone)]
struct Upstream;

impl Event for Upstream {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga_trait_state_removed"),
        TypeName::new("Upstream"),
    );
    type Codec = UnitCodec;
}

/// Correlation rule of [`MinimalSaga`]: the single process instance every
/// upstream event in this file belongs to.
const MINIMAL_SAGA_ID: &str = "saga-trait-state-removed";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Upstream(Upstream),
}

/// A saga that implements exactly the required trait items.  Its state is the
/// `applied` field — no associated type participates in holding it.
#[derive(Default)]
struct MinimalSaga {
    applied: u32,
}

#[async_trait]
impl Saga for MinimalSaga {
    type Subscribe = Observed;
    type Event = Counted;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(MINIMAL_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {
        self.applied += 1;
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
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
