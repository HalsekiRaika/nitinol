//! The process manager that turns one fact into many creations.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, TellIntent};

use crate::batch::BatchDecomposed;
use crate::item::{CreateItem, Item};
use crate::router::ItemRegistry;

/// The saga's own decision event: "this fan-out was started for this batch".
///
/// It restates the batch and the children rather than pointing back at the
/// trigger, so the saga's stream can be read on its own — the same reason the
/// trigger carries its batch.
///
/// Contrast [`ItemCreated`](crate::item::ItemCreated), which carries nothing:
/// *which* item that event is about is already its stream key, whereas *which
/// children* this fan-out covers is not derivable from the saga's stream key.
/// An event carries what its stream does not already say, and no more.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FanOutStarted {
    pub batch: String,
    pub items: Vec<String>,
}

impl Event for FanOutStarted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.fanout"), TypeName::new("FanOutStarted"));
}

/// Delivers one creation command per child named by a [`BatchDecomposed`].
pub struct FanOutSaga {
    registry: Arc<ItemRegistry>,
}

impl FanOutSaga {
    pub fn new(registry: Arc<ItemRegistry>) -> Self {
        Self { registry }
    }

    /// One fan-out process per batch.
    ///
    /// This is the single definition of that rule: [`Saga::correlate`] answers
    /// with it, and it names the stream the instance persists to.
    pub fn instance_id(batch: &str) -> SagaId {
        SagaId::new(format!("{batch}-fanout-saga"))
    }

    /// Rebuild a pending creation from the bytes a `TellRequested` marker kept
    /// for it.
    ///
    /// After a full process crash the in-memory intent is gone and only the
    /// marker survives, so the command has to be reconstructed from its
    /// serialized form — which is exactly why [`CreateItem`] carries the target
    /// stream key.
    ///
    /// `None` is the framework's signal that this marker cannot be
    /// reconstructed; replay then records a `TellFailed` for it instead of
    /// dropping it unnoticed.
    pub fn crash_restart_intent(
        registry: &Arc<ItemRegistry>,
        crash_restart_payload: &[u8],
    ) -> Option<TellIntent> {
        let cmd: CreateItem = serde_json::from_slice(crash_restart_payload).ok()?;
        let target = registry.target(&cmd.item);
        Some(TellIntent::new::<Item, CreateItem, _>(target, cmd))
    }
}

#[async_trait]
impl Saga for FanOutSaga {
    type SubscribedEvent = BatchDecomposed;
    type Event = FanOutStarted;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(Self::instance_id(&event.batch))
    }

    /// The saga keeps no state.
    ///
    /// How far the fan-out got is already recorded — each child's own stream
    /// says whether it exists.  Mirroring that into the saga would give the
    /// same fact two owners that a crash between them could leave disagreeing,
    /// and the disagreement would be invisible until the next replay.
    fn apply(&mut self, _event: Self::Event) {}

    /// A redelivered fact event produces the same decision again, on purpose.
    ///
    /// Suppressing the second fan-out here would put the idempotence in the
    /// saga, where it holds only as long as this incarnation's memory does.
    /// Leaving it in the children puts it behind their durable streams
    /// instead, so it survives the crash it exists to protect against.
    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let decision = SagaEffect::persist(FanOutStarted {
            batch: event.batch,
            items: event.items.clone(),
        });

        // `combine` folds adjacent `Persist` branches, so the decision and all
        // of its outbox markers reach the store as one append.  A crash after a
        // partial write could otherwise leave the saga believing it had fanned
        // out to children it never enqueued.
        Ok(event.items.into_iter().fold(decision, |effect, item| {
            let target = self.registry.target(&item);
            effect.combine(SagaEffect::tell::<Item, CreateItem, _>(
                target,
                CreateItem { item },
            ))
        }))
    }
}
