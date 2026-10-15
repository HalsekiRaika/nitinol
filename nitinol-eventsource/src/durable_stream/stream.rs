use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::Journal;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::SpawnError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Props, StreamProps, SupervisionStrategy};
use nitinol_runtime::ProcessSystem;

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{
    DurablePollerProcess, IntervalDriver, SharedPollState, TransformFn,
};
use crate::durable_stream::proxy::{self, DurableStreamProxy};

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
/// Typestate guarantees that `spawn` is only callable once a `SequenceCursor`
/// has been supplied via [`cursor`][DurableStream::cursor].  See the
/// `compile_fail` doctest below.
///
/// ```compile_fail
/// # use nitinol_eventsource::DurableStream;
/// # use nitinol_persistence::store::InMemoryEventStore;
/// # use nitinol_persistence::LoadedEvent;
/// # use nitinol_runtime::ident::ProcessName;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn bad() {
/// let system = ProcessSystem::new().await;
/// let store = InMemoryEventStore::default();
/// // compile error: spawn() requires CursorSet — call cursor(...) first
/// DurableStream::<()>::new(ProcessName::new("topic"), store, |_: LoadedEvent| None)
///     .spawn(&system)
///     .await
///     .unwrap();
/// # }
/// ```
pub struct DurableStream<T, S = CursorUnset> {
    topic: ProcessName,
    store: Journal,
    transform: TransformFn<T>,
    poll_interval: Duration,
    cursor: S,
    // T appears only inside the boxed transform, so we carry an explicit
    // phantom marker that preserves covariance without requiring T: Send/Sync
    // for the struct definition itself.
    _phantom: PhantomData<fn() -> T>,
}

impl<T> DurableStream<T, CursorUnset>
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
    /// The journal arrives as whatever the caller holds: a backend handle, a
    /// [`Journal`] a supplier answered with, or a reference to one the caller
    /// keeps.
    pub fn new<F>(topic: ProcessName, store: impl Into<Journal>, transform: F) -> Self
    where
        F: Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static,
    {
        Self {
            topic,
            store: store.into(),
            transform: Arc::new(transform),
            poll_interval: super::DEFAULT_POLL_INTERVAL,
            cursor: CursorUnset,
            _phantom: PhantomData,
        }
    }
}

impl<T, S> DurableStream<T, S> {
    /// Override the polling cadence.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

impl<T> DurableStream<T, CursorUnset> {
    /// Provide the starting cursor and unlock [`spawn`][DurableStream::spawn].
    pub fn cursor(self, cursor: SequenceCursor) -> DurableStream<T, CursorSet> {
        DurableStream {
            topic: self.topic,
            store: self.store,
            transform: self.transform,
            poll_interval: self.poll_interval,
            cursor: CursorSet(cursor),
            _phantom: PhantomData,
        }
    }
}

impl<T> DurableStream<T, CursorSet>
where
    T: 'static + Send + Sync + Clone,
{
    pub async fn spawn(self, system: &ProcessSystem) -> Result<DurableStreamProxy<T>, SpawnError> {
        let stream_proxy = system
            .spawn(StreamProps::<T>::new(self.topic.clone()))
            .await?;
        let publisher = stream_proxy.clone();
        let DurableStream {
            store,
            transform,
            poll_interval,
            cursor: CursorSet(initial_cursor),
            ..
        } = self;

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
        let shared_poller = system.spawn(props).await;

        Ok(DurableStreamProxy::new(
            stream_proxy,
            shared_poller,
            shared,
            proxy_store,
            poll_interval,
        ))
    }
}
