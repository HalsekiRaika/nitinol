//! Counter aggregate used as both A and B in the communication example.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Notify;

use nitinol::eventsource::Event;
use nitinol_eventsource::{
    Aggregate, AggregateProxy, Context, Decider, Effect, Receive as EvtReceive,
};

use crate::effects::TellTargetEffect;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Event)]
#[event(family = "agg_comm.counter")]
pub struct Incremented;

#[derive(Default)]
pub struct Counter {
    pub value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Plain increment command — no side effects.
pub struct Increment;

/// Read-only query.
pub struct GetCount;

/// Command that triggers a side effect: sends `Increment` to a target aggregate.
pub struct DelegateToTarget {
    pub target: AggregateProxy<Counter>,
    pub done: Arc<Notify>,
}

// ---------------------------------------------------------------------------
// Decider / Receive impls
// ---------------------------------------------------------------------------

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

#[async_trait]
impl Decider<DelegateToTarget> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: DelegateToTarget,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        // No events persisted locally — the side effect tells the target
        Ok(Effect::Side(Box::new(TellTargetEffect {
            target: cmd.target,
            done: cmd.done,
        })))
    }
}

#[async_trait]
impl EvtReceive<GetCount> for Counter {
    type Response = u64;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: GetCount, _ctx: &mut Context) -> Result<u64, Self::Error> {
        Ok(self.value)
    }
}
