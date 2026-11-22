//! What the activator keeps of a projection: everything it takes to start the
//! projection's projector again.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{DeliveryMode, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, EventType, LoadQuery, LoadedEvent};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{AnyProxy, ProcessContext};
use nitinol_runtime::{IdleTimeout, Props, SupervisionStrategy};

use super::{Progress, ProjectionActivator};
use crate::durable_stream::{DurableSubscription, Nudge, PollerLink, SequenceCursor};
use crate::projection::handler::EventTypeHandler;
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::tx_provider::ErasedTxProvider;
use crate::projection::ProjectionId;

/// What an append has to touch for a projection to be woken by it.
pub(crate) enum Interest {
    /// A projection following every stream: the event types its handlers take.
    EventTypes(Vec<EventType>),
    /// A projection following one stream: that stream, whatever is appended to
    /// it.
    Stream(Arc<str>),
}

/// A projector the activator has running, and the poller reading for it.
pub(crate) struct Running {
    pub(crate) pid: Pid,
    pub(crate) projector: AnyProxy,
    /// Has the poller read at once.
    pub(crate) nudge: Nudge,
}

/// A registered projection, with its projector type, offset store and
/// transaction erased, so the activator keeps every projection in one table.
#[async_trait]
pub(crate) trait Declaration: Send + Sync {
    fn projection_id(&self) -> &ProjectionId;

    fn interest(&self) -> Interest;

    /// How long a failed activation waits before it is tried again: the
    /// interval its poller reads at.
    fn poll_interval(&self) -> Duration;

    /// Start a projector for this projection as a child of the activator,
    /// from the position its offset store records, with `progress` settled
    /// through that position.
    async fn activate(
        &self,
        ctx: &mut ProcessContext<ProjectionActivator>,
        progress: &Progress,
    ) -> Result<Running, OffsetError>;
}

/// Everything a [`ProjectorProps`](crate::ProjectorProps) carried, and the
/// journal and offset store the system it was spawned on resolved.
pub(crate) struct ProjectorDeclaration<P, Cs, Tx> {
    pub(crate) projection_id: ProjectionId,
    /// Built once: the key this projection's position is filed under is the
    /// same for every load and save of every activation.
    pub(crate) offset_scope: OffsetScope,
    pub(crate) journal: Journal,
    pub(crate) offset_store: Cs,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) catchup_origin: CatchupOrigin,
    pub(crate) producer: Arc<dyn Fn() -> P + Send + Sync>,
    pub(crate) handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    pub(crate) tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    pub(crate) poll_interval: Duration,
    pub(crate) batch_limit: NonZeroUsize,
    pub(crate) idle_timeout: IdleTimeout,
}

impl<P, Cs, Tx> ProjectorDeclaration<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Clone + Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Settle `progress` through the global position the recorded `offset` stands at.
    ///
    /// An activation that started from zero would take every event the
    /// watcher replays for one it still owes, be woken for each, read nothing
    /// and stop, over and over. A projection following every stream keeps its
    /// offset in global positions already; one following a stream keeps the
    /// stream's own, so the record there is read once for its global position.
    async fn settle_recorded(&self, offset: u64, progress: &Progress) {
        let stream = match &self.catchup_origin {
            CatchupOrigin::Global => {
                progress.settle_through(offset);
                return;
            }
            CatchupOrigin::Aggregate(stream) => stream,
        };
        if offset == 0 {
            return;
        }
        match self.global_position_of(stream, offset).await {
            Ok(Some(global)) => progress.settle_through(global),
            Ok(None) => tracing::warn!(
                projection_id = ?self.projection_id,
                offset,
                "projection activator: the journal holds no record at the projection's \
                 recorded offset; events it settled may wake it again"
            ),
            Err(error) => tracing::warn!(
                projection_id = ?self.projection_id,
                error = %error,
                "projection activator: reading the global position of the projection's \
                 recorded offset failed; events it settled may wake it again"
            ),
        }
    }

    async fn global_position_of(
        &self,
        stream: &AggregateId,
        sequence: u64,
    ) -> Result<Option<u64>, nitinol_persistence::error::LoadError> {
        let query = LoadQuery::by_stream(stream.as_str())
            .with_from_stream_sequence(sequence)
            .with_limit(1);
        let mut records = self.journal.load(query).await?;
        match records.next().await {
            Some(Ok(record)) if record.sequence == sequence => Ok(Some(record.global_sequence)),
            Some(Ok(_)) | None => Ok(None),
            Some(Err(error)) => Err(error),
        }
    }
}

#[async_trait]
impl<P, Cs, Tx> Declaration for ProjectorDeclaration<P, Cs, Tx>
where
    P: Send + Sync + 'static,
    Cs: OffsetStore<Tx = Tx> + Clone + Send + Sync + 'static,
    Tx: Send + 'static,
{
    fn projection_id(&self) -> &ProjectionId {
        &self.projection_id
    }

    fn interest(&self) -> Interest {
        match &self.catchup_origin {
            CatchupOrigin::Global => Interest::EventTypes(
                self.handlers
                    .iter()
                    .map(|handler| handler.event_type())
                    .collect(),
            ),
            CatchupOrigin::Aggregate(stream) => Interest::Stream(Arc::from(stream.as_str())),
        }
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    async fn activate(
        &self,
        ctx: &mut ProcessContext<ProjectionActivator>,
        progress: &Progress,
    ) -> Result<Running, OffsetError> {
        let offset = self
            .offset_store
            .load(&self.offset_scope)
            .await?
            .unwrap_or(0);
        self.settle_recorded(offset, progress).await;

        let cursor = match &self.catchup_origin {
            CatchupOrigin::Aggregate(stream) => SequenceCursor::Stream {
                key: stream.as_str().to_owned(),
                after: offset,
            },
            CatchupOrigin::Global => SequenceCursor::Global { after: offset },
        };

        let producer = Arc::clone(&self.producer);
        let projection_id = self.projection_id.clone();
        let offset_scope = self.offset_scope.clone();
        let offset_store = self.offset_store.clone();
        let delivery_mode = self.delivery_mode;
        let catchup_origin = self.catchup_origin.clone();
        let handlers = self.handlers.clone();
        let tx_provider = self.tx_provider.clone();
        let progress_of_projector = progress.clone();
        let props = Props::new(move || ProjectorProcess {
            projector: producer(),
            projection_id: projection_id.clone(),
            offset_scope: offset_scope.clone(),
            offset_store: offset_store.clone(),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            handlers: handlers.clone(),
            tx_provider: tx_provider.clone(),
            offset_sequence: offset,
            progress: progress_of_projector.clone(),
        })
        // An event the projector could not apply is reported by returning
        // `Err`, which is also what supervision sees.  Under the runtime's
        // default `Stop` the first such failure would take the projector down,
        // so the event held for redelivery would not be polled again until
        // something woke the projection.  `Restart` is no better here: it
        // rebuilds the projector from `producer()`, discarding the state a
        // stateful projector accumulated and the offset it was restored with.
        .with_supervision_strategy(SupervisionStrategy::Resume)
        .with_idle_timeout(self.idle_timeout);

        let projector = ctx.spawn_child(props).await;
        ctx.watch(projector.pid()).await;

        let nudge = Nudge::default();
        let link = PollerLink {
            nudge: nudge.clone(),
            reads: Arc::new(progress.clone()),
        };
        DurableSubscription::<LoadedEvent>::new(Some)
            .with_poll_interval(self.poll_interval)
            .spawn_batch_child_for(
                ctx,
                self.journal.clone(),
                projector.clone(),
                cursor,
                self.batch_limit,
                Some(link),
            )
            .await;

        Ok(Running {
            pid: projector.pid(),
            projector: AnyProxy::from(projector),
            nudge,
        })
    }
}
