use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_persistence::store::{CheckpointStore, DeliveryMode, EventStore};
use nitinol_persistence::{AggregateId, LoadQuery, ProjectionId};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::event::Event;
use crate::projection::delivery;
use crate::projection::envelope::EventEnvelope;
use crate::projection::handler::EventTypeHandler;
use crate::projection::projector::Projector;

/// Specifies which events are loaded during the catch-up phase.
#[derive(Clone)]
pub(crate) enum CatchupOrigin {
    /// Load events for a single aggregate, filtered by aggregate sequence.
    Aggregate(AggregateId),
    /// Load all events across all aggregates, ordered by global sequence.
    Global,
}

/// Runtime `Process` that orchestrates catch-up and live event projection.
///
/// On start it performs a full catch-up from the last saved checkpoint, then
/// transitions to receiving live `EventEnvelope<E>` messages dispatched by
/// subscribed `Stream<EventEnvelope<E>>` processes.
pub struct ProjectorProcess<P, Cs>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    pub(crate) projector: P,
    pub(crate) projection_id: ProjectionId,
    pub(crate) event_store: Arc<dyn EventStore>,
    pub(crate) checkpoint_store: Arc<Cs>,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, ()>>>,
    /// Last processed sequence (aggregate or global depending on origin).
    /// Used for deduplication of live events that overlap with catch-up.
    pub(crate) checkpoint_sequence: u64,
}

impl<P, Cs> Process for ProjectorProcess<P, Cs>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    async fn on_start(&mut self, _ctx: &mut ProcessContext) {
        // 1. Load the last saved checkpoint.
        let checkpoint = match self.checkpoint_store.load(&self.projection_id).await {
            Ok(cp) => cp.unwrap_or(0),
            Err(e) => {
                tracing::error!(error = ?e, "checkpoint load failed during on_start");
                return;
            }
        };
        self.checkpoint_sequence = checkpoint;

        // 2. Build the load query, starting just after the checkpoint.
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

        // 3. Clone the Arc so the stream's lifetime is independent of `self`.
        let event_store = Arc::clone(&self.event_store);
        let mut stream = match event_store.load(query).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "event store load failed during catch-up");
                return;
            }
        };

        // 4. Process each event from the store.
        while let Some(result) = stream.next().await {
            let loaded = match result {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = ?e, "event stream error during catch-up");
                    continue;
                }
            };

            // Determine the sequence value used for checkpointing and ctx.
            let sequence = match &self.catchup_origin {
                CatchupOrigin::Aggregate(_) => loaded.sequence,
                CatchupOrigin::Global => loaded.global_sequence,
            };

            // Locate the handler for this event type; log and skip unknown types.
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

            self.checkpoint_sequence = delivery::apply(
                delivery_mode,
                checkpoint_store.as_ref(),
                &projection_id,
                current_checkpoint,
                sequence,
                |ctx| handler.handle(projector, payload, ctx),
            )
            .await;

            // Yield cooperatively so other tasks get a chance to run between
            // events during a long catch-up batch, preventing thread starvation.
            tokio::task::yield_now().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Live event receive
//
// One impl per event type E (where P: Projector<E, ()>).
// The runtime dispatches EventEnvelope<E> messages here after catch-up.
// ---------------------------------------------------------------------------

impl<P, Cs, E> Receive<EventEnvelope<E>> for ProjectorProcess<P, Cs>
where
    P: Projector<E, ()> + Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
    E: Event,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: EventEnvelope<E>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        // Determine the sequence used for deduplication based on catch-up origin.
        let event_sequence = match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => msg.sequence,
            CatchupOrigin::Global => msg.global_sequence,
        };

        // Skip events already covered by catch-up or a previous live event.
        if event_sequence <= self.checkpoint_sequence {
            return Ok(());
        }

        let projection_id = self.projection_id.clone();
        let delivery_mode = self.delivery_mode;
        let checkpoint_store = Arc::clone(&self.checkpoint_store);
        let current_checkpoint = self.checkpoint_sequence;
        let event = msg.event;
        let projector = &mut self.projector;

        self.checkpoint_sequence = delivery::apply(
            delivery_mode,
            checkpoint_store.as_ref(),
            &projection_id,
            current_checkpoint,
            event_sequence,
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
