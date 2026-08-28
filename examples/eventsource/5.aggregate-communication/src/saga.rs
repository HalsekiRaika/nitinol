//! The process manager that carries a fact from counter A to counter B.
//!
//! This is where aggregate-to-aggregate communication lives.  An aggregate
//! decides; it does not reach out.  A decision is a value — facts and an answer
//! — and a value cannot tell anybody anything, so the reaching out belongs to
//! something that reads the facts afterwards and is itself durable.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::AggregateProxy;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

use crate::counter::{Counter, Increment, Incremented};

/// The saga's own record that it has taken responsibility for one relay.
///
/// It is written to the saga's journal in the same atomic append as the outbox
/// marker for the `tell` below, which is what makes the relay survive a crash:
/// on restart the saga replays this and re-dispatches anything the marker says
/// was never delivered.
#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.saga")]
pub struct RelayRequested;

/// Relays every increment of counter A into an increment of counter B.
pub struct RelaySaga {
    pub target: AggregateProxy<Counter>,
}

impl RelaySaga {
    /// This example runs a single relay process, so its correlation rule is the
    /// one definition point of that instance's identity: `Saga::correlate`
    /// answers with it and the spawn site names the instance with it.
    pub fn instance_id() -> SagaId {
        SagaId::new("example-relay-saga")
    }
}

#[async_trait]
impl Saga for RelaySaga {
    type SubscribedEvent = Incremented;
    type Event = RelayRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(Self::instance_id())
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let persist = SagaEffect::persist(RelayRequested);
        let tell = SagaEffect::tell(self.target.clone(), Increment);
        Ok(persist.combine(tell))
    }
}
