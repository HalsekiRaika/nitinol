//! What the activator keeps of a projection: everything it takes to start the
//! projection's projector again.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nitinol_persistence::store::{DeliveryMode, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::EventType;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{AnyProxy, ProcessProxy};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props, SupervisionStrategy};

use super::ProjectionActivator;
use crate::durable_stream::ReadNow;
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

/// A projector the activator has running.
pub(crate) struct Running {
    pub(crate) pid: Pid,
    pub(crate) projector: AnyProxy,
}

/// A registered projection, with its projector type, offset store and
/// transaction erased, so the activator keeps every projection in one table.
#[async_trait]
pub(crate) trait Declaration: Send + Sync {
    fn projection_id(&self) -> &ProjectionId;

    fn interest(&self) -> Interest;

    /// How long a stop that came before anything was settled waits before the
    /// projection is activated again: the interval its poller reads at.
    fn poll_interval(&self) -> Duration;

    /// Start a projector for this projection on `system`, as a top-level
    /// process reporting to `activator`. Nothing is read here: the projector
    /// reads its own offset as it starts.
    async fn activate(
        &self,
        system: &ProcessSystem,
        activator: ProcessProxy<ProjectionActivator>,
    ) -> Running;

    /// Have `projector`, one [`activate`](Self::activate) started, read at
    /// once.
    async fn read_now(&self, projector: &AnyProxy);
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
        system: &ProcessSystem,
        activator: ProcessProxy<ProjectionActivator>,
    ) -> Running {
        let projection_id = self.projection_id.clone();
        let offset_scope = self.offset_scope.clone();
        let journal = self.journal.clone();
        let offset_store = self.offset_store.clone();
        let delivery_mode = self.delivery_mode;
        let catchup_origin = self.catchup_origin.clone();
        let producer = Arc::clone(&self.producer);
        let handlers = self.handlers.clone();
        let tx_provider = self.tx_provider.clone();
        let (poll_interval, batch_limit) = (self.poll_interval, self.batch_limit);
        let props = Props::new(move || ProjectorProcess {
            projection_id: projection_id.clone(),
            offset_scope: offset_scope.clone(),
            journal: journal.clone(),
            offset_store: offset_store.clone(),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            producer: Arc::clone(&producer),
            handlers: handlers.clone(),
            tx_provider: tx_provider.clone(),
            poll_interval,
            batch_limit,
            activator: activator.clone(),
            restored: None,
            poller: None,
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

        let projector = system.spawn(props).await;
        Running {
            pid: projector.pid(),
            projector: AnyProxy::from(projector),
        }
    }

    async fn read_now(&self, projector: &AnyProxy) {
        let projector = projector
            .downcast::<ProjectorProcess<P, Cs, Tx>>()
            .expect("the activator hands a declaration only the projector it activated");
        // A projector that is stopping refuses it; its `Terminated` follows,
        // and the activator weighs what it is owed then.
        if let Err(error) = projector.tell(ReadNow).await {
            tracing::debug!(
                projection_id = ?self.projection_id,
                error = %error,
                "projection activator: the projector is stopping; the read is not started"
            );
        }
    }
}
