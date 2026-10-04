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
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::counter::{Counter, Increment, Incremented};

/// The saga's own record that it has taken responsibility for one relay.
///
/// It is written to the saga's journal in the same atomic append as the outbox
/// marker for the `tell` below, which is what makes the relay survive a crash:
/// on restart the saga replays this and re-dispatches anything the marker says
/// was never delivered.
#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.saga", codec = JsonCodec)]
pub struct RelayRequested;

/// The whole alphabet this saga reacts to: counter A's fact, and no deadline of
/// its own.
///
/// Everything the saga reacts to is declared here, and everything reaches it
/// through the one [`Saga::handle`] entry point.  A relay that later had to
/// notice B's reply, or a timeout of its own, would gain a variant here — and
/// the `match` in `handle` would stop compiling until it was answered.
#[derive(SagaSubscribe)]
pub enum Observed {
    Incremented(Incremented),
}

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
    type Subscribe = Observed;
    type Event = RelayRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(Self::instance_id())
    }

    // `apply` is left to its default: the relay keeps no state.  Whether B was
    // incremented is recorded on B's own stream, and the outbox marker on the
    // saga's journal is what says whether the dispatch still owes delivery.

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        // The alphabet has one variant, so this binding is exhaustive — and it
        // is what a second variant would make the compiler reject.  A's fact
        // carries nothing the relay needs: that it happened is the whole input.
        let Observed::Incremented(_fact) = input;
        let persist = SagaEffect::persist(RelayRequested);
        let tell = SagaEffect::tell(self.target.clone(), Increment);
        Ok(persist.combine(tell))
    }
}
