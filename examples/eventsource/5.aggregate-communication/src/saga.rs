//! The process manager that carries a fact from counter A to counter B.
//!
//! This is where aggregate-to-aggregate communication lives.  An aggregate
//! decides; it does not reach out.  A decision is a value — facts and an answer
//! — and a value cannot tell anybody anything, so the reaching out belongs to
//! something that reads the facts afterwards and is itself durable.
//!
//! That something is split in two, and the split is the whole lesson:
//!
//! * the **saga** observes A's stream and persists its own decision that a
//!   relay is owed.  That is all it can do — a [`SagaEffect`] has no "send this
//!   command" variant — so the decision is either a row on the saga's stream or
//!   it never happened;
//! * [`Saga::dispatch`] is the pure mapping from one such decision to the
//!   commands it implies, and a **delivery host** (wired in `src/main.rs`)
//!   reads the decisions out of the store and `ask`s the aggregates they are
//!   addressed to.
//!
//! Deciding and delivering are therefore two writes to two places, with no
//! transaction spanning them — which is why delivery is at-least-once rather
//! than exactly-once, and why the decision below carries an operation id.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_persistence::AggregateId;
use nitinol_saga::{Saga, SagaContext, SagaDelivery, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::counter::{Counter, Increment, Incremented};

/// The saga's own record that it has taken responsibility for one relay.
///
/// A delivery host reads this event and nothing else — not the saga, not the
/// fact that triggered it — so everything the delivery needs is on it:
///
/// * `target` — the stream key of the counter the relay is addressed to.  The
///   saga is told which counter at spawn time and writes that address down
///   here, because a host reads this row long after the incarnation that wrote
///   it is gone and has no live reference to recover.
/// * `operation` — the id by which the receiving counter recognises a repeat.
///   It is derived from the position of A's fact, so deciding the same input
///   again yields the same id.
///
/// Persisting it is also what makes the relay survive a crash: the host's
/// offset only moves past a decision once the commands it implied have
/// landed, so a host that died in between resumes from the offset it last
/// recorded and carries this decision again.
#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.saga", codec = JsonCodec)]
pub struct RelayRequested {
    pub target: String,
    pub operation: String,
}

/// The whole alphabet this saga reacts to: counter A's fact, and no deadline of
/// its own.
///
/// Everything the saga reacts to is declared here, and everything reaches it
/// through the one [`Saga::handle`] entry point.  A relay that later had to
/// notice B's reply, or a timeout of its own, would gain a variant here — and
/// the `match` in `handle` would stop compiling until it was answered.  That is
/// also where a delivery B *refused* would be answered: a refusal is a domain
/// fact on B's own stream, observed like any other input, never a callback from
/// the delivery machinery.
#[derive(SagaSubscribe)]
pub enum Observed {
    Incremented(Incremented),
}

/// Relays every increment of counter A into an increment of counter B.
pub struct RelaySaga {
    /// Which counter the relay is addressed to.  An address, not a live proxy:
    /// it is what the decision carries, and the decision is the host's whole
    /// input.
    pub target: AggregateId,
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
    // incremented is recorded on B's own stream, and how far the relays have
    // been carried out is the delivery host's offset — neither is the
    // saga's to mirror.

    /// What one relay decision means for everybody else.
    ///
    /// A pure associated function of the decision alone — no `&self`, because a
    /// delivery host keeps no saga resident: it reads decisions out of the
    /// store, including ones persisted by an incarnation that no longer exists,
    /// so there is nothing for it to ask.  Everything it needs is on the event,
    /// which is why [`RelayRequested`] carries the address and the operation id.
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Counter, Increment>(
            AggregateId::new(event.target.clone()),
            Increment {
                operation: event.operation.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        // The alphabet has one variant, so this binding is exhaustive — and it
        // is what a second variant would make the compiler reject.  A's fact
        // carries nothing the relay needs: that it happened is the whole input.
        let Observed::Incremented(_fact) = input;

        // The operation id names the input this relay follows from, so the same
        // record decided again — after a restart, or after a redelivery of A's
        // fact — asks B for the same operation, and B recognises it.
        let operation = format!(
            "relay-of-{}#{}",
            ctx.upstream_aggregate_id().as_str(),
            ctx.upstream_sequence()
        );

        Ok(SagaEffect::persist(RelayRequested {
            target: self.target.as_str().to_owned(),
            operation,
        }))
    }
}
