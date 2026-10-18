use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::provider::Store;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::SpawnError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Props, StreamProps, SupervisionStrategy};

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{
    DurablePollerProcess, IntervalDriver, SharedPollState, TransformFn,
};
use crate::durable_stream::proxy::{self, DurableStreamProxy};
use crate::system::{EventSourceSystem, Hosted, Unhosted};

const POLLER_RESTART_MAX_RETRIES: u32 = 5;
const POLLER_RESTART_WITHIN: Duration = Duration::from_secs(60);

/// Typestate marker: the builder has no cursor configured yet.
///
/// [`DurableStream::spawn`] is unavailable in this state.
pub struct CursorUnset;

/// Typestate marker: a [`SequenceCursor`] has been supplied, so the builder
/// can be turned into a running stream via [`DurableStream::spawn`].
pub struct CursorSet(SequenceCursor);

/// Builder + spawn entry-point for an at-least-once event stream backed by an
/// [`EventStore`](nitinol_persistence::store::EventStore) poller.
///
/// What it carries is the topic to publish on, how to read each record and how
/// often to ask — never *which* journal. That comes from the system the stream
/// is spawned on: `H` is whether one has been named yet, with [`Unhosted`]
/// props taking a system at [`spawn`][DurableStream::spawn] and props from
/// [`EventSourceSystem::durable_stream`] carrying it already. Both forms meet on
/// one private path and read the same journal.
///
/// Typestate guarantees that `spawn` is only callable once a `SequenceCursor`
/// has been supplied via [`cursor`][DurableStream::cursor].  See the
/// `compile_fail` doctest below.
///
/// ```compile_fail
/// # use nitinol_eventsource::DurableStream;
/// # use nitinol_eventsource::system::EventSourceSystem;
/// # use nitinol_persistence::provider::PersistenceProvider;
/// # use nitinol_persistence::store::InMemoryStore;
/// # use nitinol_persistence::LoadedEvent;
/// # use nitinol_runtime::ident::ProcessName;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn bad() {
/// # let ps = ProcessSystem::new().await;
/// let system = EventSourceSystem::builder(ps)
///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
///     .build();
/// // compile error: spawn() requires CursorSet — call cursor(...) first
/// DurableStream::<()>::new(ProcessName::new("topic"), |_: LoadedEvent| None)
///     .spawn(&system)
///     .await
///     .unwrap();
/// # }
/// ```
///
/// Nor is there a form that names a journal. A stream over a different
/// destination is a second system over the supplier that holds it:
///
/// ```compile_fail
/// # use nitinol_eventsource::DurableStream;
/// # use nitinol_persistence::store::InMemoryEventStore;
/// # use nitinol_persistence::LoadedEvent;
/// # use nitinol_runtime::ident::ProcessName;
/// # fn bad() {
/// let store = InMemoryEventStore::default();
/// DurableStream::<()>::new(ProcessName::new("topic"), store, |_: LoadedEvent| None);
/// # }
/// ```
pub struct DurableStream<T, S = CursorUnset, H = Unhosted> {
    topic: ProcessName,
    transform: TransformFn<T>,
    poll_interval: Duration,
    cursor: S,
    host: H,
    // T appears only inside the boxed transform, so we carry an explicit
    // phantom marker that preserves covariance without requiring T: Send/Sync
    // for the struct definition itself.
    _phantom: PhantomData<fn() -> T>,
}

impl<T> DurableStream<T, CursorUnset, Unhosted>
where
    T: 'static + Send + Sync,
{
    /// Begin building a durable stream that publishes onto a `Stream<T>`
    /// registered under `topic`.
    ///
    /// `transform` maps each loaded event into the stream's payload type.
    /// Returning `None` skips the event but advances the cursor, which
    /// prevents the same payload from being re-loaded on every poll.
    ///
    /// Which journal is polled is settled at the spawn, by the system this is
    /// spawned on.
    pub fn new<F>(topic: ProcessName, transform: F) -> Self
    where
        F: Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static,
    {
        Self {
            topic,
            transform: Arc::new(transform),
            poll_interval: super::DEFAULT_POLL_INTERVAL,
            cursor: CursorUnset,
            host: Unhosted,
            _phantom: PhantomData,
        }
    }
}

impl<T, S, H> DurableStream<T, S, H> {
    /// Override the polling cadence.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

impl<T, S> DurableStream<T, S, Unhosted> {
    /// Carry `host` as where this stream starts.
    pub(crate) fn hosted<St>(self, host: EventSourceSystem<St>) -> DurableStream<T, S, Hosted<St>> {
        DurableStream {
            topic: self.topic,
            transform: self.transform,
            poll_interval: self.poll_interval,
            cursor: self.cursor,
            host: Hosted::new(host),
            _phantom: PhantomData,
        }
    }
}

impl<T, H> DurableStream<T, CursorUnset, H> {
    /// Provide the starting cursor and unlock [`spawn`][DurableStream::spawn].
    pub fn cursor(self, cursor: SequenceCursor) -> DurableStream<T, CursorSet, H> {
        DurableStream {
            topic: self.topic,
            transform: self.transform,
            poll_interval: self.poll_interval,
            cursor: CursorSet(cursor),
            host: self.host,
            _phantom: PhantomData,
        }
    }
}

impl<T> DurableStream<T, CursorSet, Unhosted>
where
    T: 'static + Send + Sync + Clone,
{
    /// Start this stream over the journal `system` distributes.
    pub async fn spawn<St: Store>(
        self,
        system: &EventSourceSystem<St>,
    ) -> Result<DurableStreamProxy<T>, SpawnError> {
        self.start_on(system).await
    }
}

impl<T, St> DurableStream<T, CursorSet, Hosted<St>>
where
    T: 'static + Send + Sync + Clone,
    St: Store,
{
    /// Start this stream over the journal the system it came from distributes.
    pub async fn spawn(self) -> Result<DurableStreamProxy<T>, SpawnError> {
        let host = self.host.system().clone();
        self.start_on(&host).await
    }
}

impl<T, H> DurableStream<T, CursorSet, H>
where
    T: 'static + Send + Sync + Clone,
{
    /// The one road both entry points take.
    ///
    /// The journal and the process system are taken from `system` here and
    /// nowhere else, so the two forms cannot drift into polling different
    /// stores.
    async fn start_on<St: Store>(
        self,
        system: &EventSourceSystem<St>,
    ) -> Result<DurableStreamProxy<T>, SpawnError> {
        let ps = system.process_system();
        let stream_proxy = ps.spawn(StreamProps::<T>::new(self.topic.clone())).await?;
        let publisher = stream_proxy.clone();
        let DurableStream {
            transform,
            poll_interval,
            cursor: CursorSet(initial_cursor),
            ..
        } = self;

        let store = system.persistence().event_store();
        let proxy_store = store.clone();

        let shared = SharedPollState::new(transform);
        let producer_store = store;
        let producer_shared = shared.clone();
        let producer_publisher = publisher.clone();
        let producer_cursor = initial_cursor;

        let restart_strategy =
            SupervisionStrategy::restart(POLLER_RESTART_MAX_RETRIES, POLLER_RESTART_WITHIN)
                .expect("POLLER_RESTART_WITHIN is a positive duration constant");

        let driver = IntervalDriver::<DurablePollerProcess<T>>::new(poll_interval);
        let poller_name = proxy::shared_poller_name(stream_proxy.pid());
        let props = Props::new(move || DurablePollerProcess {
            store: producer_store.clone(),
            publisher: producer_publisher.clone(),
            shared: producer_shared.clone(),
            cursor: producer_cursor.clone(),
        })
        .with_supervision_strategy(restart_strategy)
        .with_name(poller_name)
        .with_driver(driver);
        let shared_poller = ps.spawn(props).await;

        Ok(DurableStreamProxy::new(
            stream_proxy,
            shared_poller,
            shared,
            proxy_store,
            poll_interval,
        ))
    }
}
