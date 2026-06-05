use std::sync::Arc;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::{AggregateId, LoadedEvent, ProjectionId};
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::durable_stream::{DurableSubscription, SequenceCursor};
use crate::projection::delivery;
use crate::projection::handler::EventTypeHandler;
use crate::projection::tx_provider::ErasedTxProvider;

/// Specifies which sequence dimension drives checkpoint advancement.
#[derive(Clone)]
pub(crate) enum CatchupOrigin {
    /// Use the per-aggregate `sequence` (stream-scoped cursor).
    Aggregate(AggregateId),
    /// Use the cross-aggregate `global_sequence`.
    Global,
}

pub struct ProjectorProcess<P, Cs, Tx = ()> {
    pub(crate) projector: P,
    pub(crate) projection_id: ProjectionId,
    pub(crate) checkpoint_store: Arc<Cs>,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) checkpoint_sequence: u64,
    pub(crate) upstream_config: DurableSubscription<LoadedEvent>,
    pub(crate) upstream_cursor: SequenceCursor,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        self.upstream_config
            .spawn_child(ctx, self.upstream_cursor.clone())
            .await;
    }
}

impl<P, Cs, Tx> Receive<LoadedEvent> for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        loaded: LoadedEvent,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let event_sequence = match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => loaded.sequence,
            CatchupOrigin::Global => loaded.global_sequence,
        };

        if event_sequence <= self.checkpoint_sequence {
            return Ok(());
        }

        let handler = match self
            .handlers
            .iter()
            .find(|h| h.event_type() == loaded.event_type)
        {
            Some(h) => Arc::clone(h),
            None => {
                tracing::debug!(
                    event_type = ?loaded.event_type,
                    "skipping unregistered event type",
                );
                return Ok(());
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
            event_sequence,
            tx_provider,
            |ctx| handler.handle(projector, payload, ctx),
        )
        .await;

        tokio::task::yield_now().await;

        Ok(())
    }
}
