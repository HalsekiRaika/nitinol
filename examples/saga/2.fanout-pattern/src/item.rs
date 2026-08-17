//! The child aggregate the fan-out creates.
//!
//! Creation is the only thing that happens here, and it is idempotent: that is
//! what lets the pattern deliver at-least-once and still need no compensation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, Receive};
use nitinol_persistence::{EventType, Family, TypeName};

/// The child's genesis event.
///
/// It carries no payload because it has nothing to say that the stream it is
/// written to does not already say: the item's identity *is* its stream key.
/// Repeating the key inside the payload would make the same fact readable from
/// two places that could disagree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemCreated;

impl Event for ItemCreated {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.fanout"), TypeName::new("ItemCreated"));
}

/// Create the item this stream stands for.
///
/// `item` is the target's stream key.  It is not read by [`Decider::decide`] —
/// the aggregate already knows which stream it is — but it is what makes the
/// command routable on its own: [`crate::router::ItemTarget`] resolves the
/// child process from it, and after a full process crash the saga's
/// crash-restart factory rebuilds that target out of these very bytes (see
/// [`crate::saga::FanOutSaga::crash_restart_intent`]).
///
/// `Clone + Serialize + Deserialize` are required by
/// `SagaEffect::tell`, which serializes the command into the
/// `TellRequested` outbox marker as its crash-restart payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateItem {
    pub item: String,
}

/// The child.
///
/// `created` is the whole state, and it is what makes a redelivered
/// `CreateItem` a no-op.
#[derive(Default)]
pub struct Item {
    created: bool,
}

impl Aggregate for Item {
    type Event = ItemCreated;

    fn apply(&mut self, _event: ItemCreated) {
        self.created = true;
    }
}

#[async_trait]
impl Decider<CreateItem> for Item {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: CreateItem,
        _ctx: &mut Context,
    ) -> Result<Effect<ItemCreated>, Self::Rejection> {
        if self.created {
            // Already created — the second delivery of a creation is the
            // success answer, not a conflict to compensate.  A resident child
            // answers it from `created`; a child restored by replay answers it
            // from its own stream, because replay is what sets `created`.
            return Ok(Effect::empty());
        }
        Ok(Effect::persist(ItemCreated))
    }
}

/// Ask a child whether it exists yet.
pub struct IsCreated;

#[async_trait]
impl Receive<IsCreated> for Item {
    type Response = bool;
    type Error = std::convert::Infallible;

    async fn recv(&self, _msg: IsCreated, _ctx: &mut Context) -> Result<bool, Self::Error> {
        Ok(self.created)
    }
}
