use std::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;
use nitinol_persistence::store::{DeliveryMode, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadedEvent};

use crate::projection::ProjectionId;
use nitinol_runtime::process::{Process, ProcessContext, Receive};

use crate::durable_stream::{DurableSubscription, PolledBatch, SequenceCursor, SettledThrough};
use crate::error::ProjectionNotSettled;
use crate::projection::delivery::{self, Batch, BatchOutcome, BatchRecord, ProjectionIdentity};
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

/// What a record to be projected is handed to its handler as.
type Projection<P, Tx> = (Arc<dyn EventTypeHandler<P, Tx>>, Bytes);

/// The running projector: it receives what each read of its journal returned
/// as one batch, and settles that batch as one unit under its delivery mode.
pub struct ProjectorProcess<P, Cs, Tx = ()> {
    pub(crate) projector: P,
    pub(crate) projection_id: ProjectionId,
    /// Built once, when this projector is spawned: the key its position is
    /// filed under never changes while it runs.
    pub(crate) offset_scope: OffsetScope,
    /// The journal this projection follows, as the system it was spawned on
    /// distributes it.
    ///
    /// Held so the poller started in `on_start` is handed the destination the
    /// spawn resolved, rather than one the subscription carries.
    pub(crate) store: Journal,
    pub(crate) offset_store: Cs,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) offset_sequence: u64,
    pub(crate) upstream_config: DurableSubscription<LoadedEvent>,
    pub(crate) upstream_cursor: SequenceCursor,
    pub(crate) batch_limit: NonZeroUsize,
}

impl<P, Cs, Tx> Process for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        self.upstream_config
            .spawn_batch_child(
                ctx,
                self.store.clone(),
                self.upstream_cursor.clone(),
                self.batch_limit,
            )
            .await;
    }
}

impl<P, Cs, Tx> ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Whether `loaded` is owed a handler's run, and which one.
    fn classify(&self, loaded: LoadedEvent) -> BatchRecord<Projection<P, Tx>> {
        let position = match &self.catchup_origin {
            CatchupOrigin::Aggregate(_) => loaded.sequence,
            CatchupOrigin::Global => loaded.global_sequence,
        };

        // Already applied by this or an earlier incarnation.  Nothing is owed
        // for it, and reporting it as unsettled would hold the cursor on a
        // redelivery that can never produce anything new.
        if position <= self.offset_sequence {
            return BatchRecord::Skipped { position };
        }

        match self
            .handlers
            .iter()
            .find(|h| h.event_type() == loaded.event_type)
        {
            Some(h) => BatchRecord::ToProject {
                position,
                item: (Arc::clone(h), loaded.payload),
            },
            None => {
                // No handler is registered for this type, so there is no
                // application to fail and nothing a redelivery would change.
                // Holding the cursor here would stall every event behind it.
                tracing::debug!(
                    event_type = ?loaded.event_type,
                    "skipping unregistered event type",
                );
                BatchRecord::Skipped { position }
            }
        }
    }
}

impl<P, Cs, Tx> Receive<PolledBatch<LoadedEvent>> for ProjectorProcess<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// How far the batch settled; the poller moves the subscription cursor
    /// there, so the records behind a failure are redelivered.
    type Response = SettledThrough;
    /// Returning `Err` stops the poller (via `ask()`) from advancing the
    /// subscription cursor at all, so a batch this projector could not settle
    /// any of is redelivered from its first record rather than skipped for
    /// good.
    type Error = ProjectionNotSettled;

    async fn recv(
        &mut self,
        batch: PolledBatch<LoadedEvent>,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<SettledThrough, ProjectionNotSettled> {
        // The poller observed each record in the dimension its cursor was
        // built for from this projector's own origin; the position is read off
        // the record here, in the dimension this projector's offset is kept in.
        let records = batch
            .records
            .into_iter()
            .map(|(_observed, loaded)| self.classify(loaded))
            .collect();

        let outcome = delivery::apply_batch(
            self.delivery_mode,
            &self.offset_store,
            ProjectionIdentity {
                id: &self.projection_id,
                scope: &self.offset_scope,
            },
            Batch {
                records,
                last_observed: batch.last_observed,
            },
            self.tx_provider.as_deref(),
            &mut self.projector,
            |projector, (handler, payload), ctx| {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    handler.handle(projector, payload, ctx).await
                })
            },
        )
        .await;

        // Both the offset this projector carries forward and the answer the
        // poller reads come from this one outcome.  Deciding them separately is
        // how the cursor came to move past events the offset had not.
        match outcome {
            BatchOutcome::Settled { through } => {
                // A batch of nothing but records the offset already covers
                // settles through a position at or below it; the offset never
                // moves back.
                self.offset_sequence = self.offset_sequence.max(through);
                Ok(SettledThrough(through))
            }
            BatchOutcome::Unsettled => Err(ProjectionNotSettled),
        }
    }
}
