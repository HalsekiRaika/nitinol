use std::sync::Arc;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::{AggregateId, LoadQuery, ProjectionId};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::event::Event;
use crate::process::persistence::EventPersistorProxy;
use crate::projection::delivery;
use crate::projection::envelope::EventEnvelope;
use crate::projection::handler::EventTypeHandler;
use crate::projection::projector::Projector;
use crate::projection::tx_provider::ErasedTxProvider;

/// Specifies which events are loaded during the catch-up phase.
#[derive(Clone)]
pub(crate) enum CatchupOrigin {
    /// Load events for a single aggregate, filtered by aggregate sequence.
    Aggregate(AggregateId),
    /// Load all events across all aggregates, ordered by global sequence.
    Global,
}

pub struct ProjectorProcess<P, Cs, Tx = ()> {
    pub(crate) projector: P,
    pub(crate) projection_id: ProjectionId,
    pub(crate) event_ref: EventPersistorProxy,
    pub(crate) checkpoint_store: Arc<Cs>,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) checkpoint_sequence: u64,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        let checkpoint = match self.checkpoint_store.load(&self.projection_id).await {
            Ok(cp) => cp.unwrap_or(0),
            Err(e) => {
                tracing::error!(error = ?e, "checkpoint load failed during on_start");
                return;
            }
        };
        self.checkpoint_sequence = checkpoint;

        let query = match &self.catchup_origin {
            CatchupOrigin::Aggregate(agg_id) => LoadQuery {
                aggregate_id: Some(agg_id.clone()),
                from_aggregate_sequence: Some(checkpoint + 1),
                ..Default::default()
            },
            CatchupOrigin::Global => LoadQuery {
                from_global_sequence: Some(checkpoint + 1),
                ..Default::default()
            },
        };

        let events = match self.event_ref.load(query).await {
            Ok(evts) => evts,
            Err(e) => {
                tracing::error!(error = ?e, "event store load failed during catch-up");
                return;
            }
        };

        for loaded in events {
            let sequence = match &self.catchup_origin {
                CatchupOrigin::Aggregate(_) => loaded.sequence,
                CatchupOrigin::Global => loaded.global_sequence,
            };

            let handler = match self
                .handlers
                .iter()
                .find(|h| h.event_type() == loaded.event_type)
            {
                Some(h) => Arc::clone(h),
                None => {
                    tracing::debug!(
                        event_type = ?loaded.event_type,
                        "skipping unregistered event type during catch-up",
                    );
                    continue;
                }
            };

            let projection_id = self.projection_id.clone();
            let delivery_mode = self.delivery_mode;
            let checkpoint_store = Arc::clone(&self.checkpoint_store);
            let current_checkpoint = self.checkpoint_sequence;
            let payload = loaded.payload;
            let projector = &mut self.projector;
            let tx_provider = self.tx_provider.as_deref();

            self.checkpoint_sequence = delivery::apply(
                delivery_mode,
                checkpoint_store.as_ref(),
                &projection_id,
                current_checkpoint,
                sequence,
                tx_provider,
                |ctx| handler.handle(projector, payload, ctx),
            )
            .await;

            tokio::task::yield_now().await;
        }
    }
}

impl<P, Cs, Tx, E> Receive<EventEnvelope<E>> for ProjectorProcess<P, Cs, Tx>
where
    P: Projector<E, Tx> + Send + Sync + 'static,
    Cs: CheckpointStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
    E: Event,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<E>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        let event_sequence = match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => msg.sequence,
            CatchupOrigin::Global => msg.global_sequence,
        };

        if event_sequence <= self.checkpoint_sequence {
            return Ok(());
        }

        let projection_id = self.projection_id.clone();
        let delivery_mode = self.delivery_mode;
        let checkpoint_store = Arc::clone(&self.checkpoint_store);
        let current_checkpoint = self.checkpoint_sequence;
        let event = msg.event;
        let projector = &mut self.projector;
        let tx_provider = self.tx_provider.as_deref();

        self.checkpoint_sequence = delivery::apply(
            delivery_mode,
            checkpoint_store.as_ref(),
            &projection_id,
            current_checkpoint,
            event_sequence,
            tx_provider,
            move |ctx| {
                Box::pin(async move {
                    projector
                        .project(event, ctx)
                        .await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                })
            },
        )
        .await;

        Ok(())
    }
}
