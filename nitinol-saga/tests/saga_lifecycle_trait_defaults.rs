//! Which `Saga` methods carry a working default: a user can implement the
//! trait by naming its associated types, `correlate` and `handle`, and nothing
//! else.
//!
//! - `apply(..)` defaults to a no-op that leaves the saga's state untouched — a
//!   translating saga whose every decision follows from its input alone has no
//!   state to fold its own events into, and writing an empty method for it
//!   would be ceremony with no reader.
//!
//! [`StubSaga`] below overrides it, so this file compiling at all is half of
//! what it pins: the test pins what the default actually does when the
//! framework calls it.
//!
//! `dispatch(..)` carries a default too — a saga whose decisions are not
//! commands for anyone maps none of them to a delivery — and what that default
//! does is pinned where delivery is observed, in the `saga_delivery_*.rs`
//! tests.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};

// A saga that implements ONLY the required members, leaving `apply` at its
// trait default.

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StubEvent;

impl Event for StubEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("trait_defaults"), TypeName::new("StubEvent"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StubUpstream;

impl Event for StubUpstream {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("trait_defaults"), TypeName::new("StubUpstream"));
    type Codec = JsonCodec;
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
/// A saga with no deadline variant declares no own time, which is the other
/// half of "nothing optional was named here".
#[derive(SagaSubscribe)]
enum Observed {
    Upstream(StubUpstream),
}

/// Correlation rule of [`StubSaga`]: the one instance the defaults below are
/// exercised against.  `correlate` has no default body, so even a saga that is
/// never spawned — these tests call its methods directly — must declare one.
const STUB_SAGA_ID: &str = "trait-defaults-stub";

/// State the default `apply` must not touch.
///
/// `folded` is what an overriding `apply` would move; `StubSaga` does not
/// override, so it is the observation that the trait's own body folded nothing
/// in.
#[derive(Default, Debug, PartialEq)]
struct StubSaga {
    folded: usize,
}

#[async_trait]
impl Saga for StubSaga {
    type Subscribe = Observed;
    type Event = StubEvent;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(STUB_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// The default `apply` implementation must be a no-op: a saga that does not
/// override it holds no state folded out of its own events, so replaying one of
/// them leaves the instance exactly as it was.
#[test]
fn default_apply_leaves_the_saga_state_untouched() {
    let mut saga = StubSaga::default();

    saga.apply(StubEvent);

    assert_eq!(
        saga,
        StubSaga::default(),
        "the default apply must fold nothing into the saga's state"
    );
}
