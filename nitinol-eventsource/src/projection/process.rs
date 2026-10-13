use std::sync::Arc;

use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadedEvent};

use crate::projection::ProjectionId;
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::durable_stream::{DurableSubscription, SequenceCursor};
use crate::error::ProjectionNotSettled;
use crate::projection::delivery::{self, ApplyOutcome, ProjectionIdentity};
use crate::projection::handler::EventTypeHandler;
use crate::projection::tx_provider::ErasedTxProvider;

/// Specifies which sequence dimension drives offset advancement.
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
    /// Built once, when this projector is spawned: the key its position is
    /// filed under never changes while it runs.
    pub(crate) offset_scope: OffsetScope,
    pub(crate) offset_store: Arc<Cs>,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) offset_sequence: u64,
    pub(crate) upstream_config: DurableSubscription<LoadedEvent>,
    pub(crate) upstream_cursor: SequenceCursor,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
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
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    type Response = ();
    /// Returning `Err` stops the `DirectPoller` (via `ask()`) from advancing the
    /// subscription cursor, so an event this projector could not apply is
    /// redelivered rather than skipped for good.
    type Error = ProjectionNotSettled;

    async fn recv(
        &mut self,
        loaded: LoadedEvent,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), ProjectionNotSettled> {
        let event_sequence = match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => loaded.sequence,
            CatchupOrigin::Global => loaded.global_sequence,
        };

        // Already applied by this or an earlier incarnation.  Nothing is owed
        // for it, and reporting it as unsettled would hold the cursor on a
        // redelivery that can never produce anything new.
        if event_sequence <= self.offset_sequence {
            return Ok(());
        }

        let handler = match self
            .handlers
            .iter()
            .find(|h| h.event_type() == loaded.event_type)
        {
            Some(h) => Arc::clone(h),
            None => {
                // No handler is registered for this type, so there is no
                // application to fail and nothing a redelivery would change.
                // Holding the cursor here would stall every event behind it.
                tracing::debug!(
                    event_type = ?loaded.event_type,
                    "skipping unregistered event type",
                );
                return Ok(());
            }
        };

        let projection_id = self.projection_id.clone();
        let delivery_mode = self.delivery_mode;
        let offset_store = Arc::clone(&self.offset_store);
        let current_offset = self.offset_sequence;
        let payload = loaded.payload;
        let offset_scope = &self.offset_scope;
        let projector = &mut self.projector;
        let tx_provider = self.tx_provider.as_deref();

        let outcome = delivery::apply(
            delivery_mode,
            offset_store.as_ref(),
            ProjectionIdentity {
                id: &projection_id,
                scope: offset_scope,
            },
            current_offset,
            event_sequence,
            tx_provider,
            |ctx| handler.handle(projector, payload, ctx),
        )
        .await;

        // Both the offset this projector carries forward and the answer the
        // poller reads come from this one outcome.  Deciding them separately is
        // how the cursor came to move past events the offset had not.
        self.offset_sequence = outcome.offset();

        tokio::task::yield_now().await;

        match outcome {
            ApplyOutcome::Settled { .. } => Ok(()),
            ApplyOutcome::Retry { .. } => Err(ProjectionNotSettled),
        }
    }
}
