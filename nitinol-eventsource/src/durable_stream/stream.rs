use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::SpawnError;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::{Props, SupervisionStrategy};
use nitinol_runtime::ProcessSystem;

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{DurablePollerProcess, IntervalDriver, TransformFn};
use crate::durable_stream::proxy::{shared_poller_name, DurableStreamProxy};

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
/// [`EventStore`] poller.
///
/// Typestate guarantees that `spawn` is only callable once a `SequenceCursor`
/// has been supplied via [`cursor`][DurableStream::cursor].  See the
/// `compile_fail` doctest below.
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use nitinol_eventsource::DurableStream;
/// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
/// # use nitinol_persistence::LoadedEvent;
/// # use nitinol_runtime::ident::ProcessName;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn bad() {
/// let system = ProcessSystem::new().await;
/// let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
/// // compile error: spawn() requires CursorSet — call cursor(...) first
/// DurableStream::<()>::new(ProcessName::new("topic"), store, |_: LoadedEvent| None)
///     .spawn(&system)
///     .await
///     .unwrap();
/// # }
/// ```
pub struct DurableStream<T, S = CursorUnset> {
    topic: ProcessName,
    store: Arc<dyn EventStore>,
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
    pub fn new<F>(topic: ProcessName, store: Arc<dyn EventStore>, transform: F) -> Self
    where
        F: Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static,
    {
        Self {
            topic,
            store,
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
        let stream_proxy = system.spawn_stream::<T>(self.topic).await?;
        let publisher = stream_proxy.clone();
        let DurableStream {
            store,
            transform,
            poll_interval,
            cursor: CursorSet(initial_cursor),
            ..
        } = self;

        let proxy_store = Arc::clone(&store);
        let proxy_transform = Arc::clone(&transform);

        let start_open = Arc::new(AtomicBool::new(false));
        let producer_store = Arc::clone(&store);
        let producer_transform = Arc::clone(&transform);
        let producer_publisher = publisher.clone();
        let producer_start_open = Arc::clone(&start_open);
        let producer_cursor = initial_cursor;

        let mut props = Props::new(move || DurablePollerProcess {
            store: Arc::clone(&producer_store),
            publisher: producer_publisher.clone(),
            transform: Arc::clone(&producer_transform),
            cursor: producer_cursor.clone(),
            start_open: Arc::clone(&producer_start_open),
        });
        props.with_supervision_strategy(SupervisionStrategy::Restart {
            max_retries: POLLER_RESTART_MAX_RETRIES,
            within: POLLER_RESTART_WITHIN,
        });

        let driver = IntervalDriver::<DurablePollerProcess<T>>::new(poll_interval);
        let poller_name = shared_poller_name(stream_proxy.pid());
        let shared_poller = system
            .spawn_named_with_driver(poller_name, props, driver)
            .await;

        Ok(DurableStreamProxy::new(
            stream_proxy,
            shared_poller,
            start_open,
            proxy_store,
            proxy_transform,
            poll_interval,
        ))
    }
}
