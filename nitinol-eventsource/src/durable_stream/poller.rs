use std::future::Future;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use nitinol_persistence::store::Journal;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, Receive, Terminated,
};
use nitinol_runtime::Stream;

use crate::durable_stream::cursor::SequenceCursor;

pub(crate) type TransformFn<T> = Arc<dyn Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static>;

/// What a shared poller and the handle onto its stream both read: the transform
/// each loaded event goes through, and whether the stream has been subscribed
/// to yet.
///
/// # Why the gate is here rather than in the poller
///
/// A poller that published before anyone subscribed would advance its cursor
/// over events it dropped, so the first subscriber would read an empty stream
/// rather than the rows that were there all along. The gate closes that window:
/// the poller publishes nothing until a `subscribe` opens it, and the `Release`
/// store there is what makes the subscription visible to the `Acquire` load the
/// poller gates on.
///
/// # Why it is one cheap-clone value
///
/// The poller's declaration is rebuilt on every supervised restart, so a
/// restarted poller has to find the gate its stream's handle already opened —
/// which it does because a clone of this names the one state rather than a copy
/// of it.
pub(crate) struct SharedPollState<T> {
    inner: Arc<SharedPollStateInner<T>>,
}

struct SharedPollStateInner<T> {
    transform: TransformFn<T>,
    start_open: AtomicBool,
}

impl<T> Clone for SharedPollState<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> SharedPollState<T> {
    pub(crate) fn new(transform: TransformFn<T>) -> Self {
        Self {
            inner: Arc::new(SharedPollStateInner {
                transform,
                start_open: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn transform(&self) -> &TransformFn<T> {
        &self.inner.transform
    }

    /// Whether the stream has been subscribed to, and so whether the poller may
    /// publish.
    pub(crate) fn is_open(&self) -> bool {
        self.inner.start_open.load(Ordering::Acquire)
    }

    /// Report the first subscription, releasing the poller to publish.
    pub(crate) fn open(&self) {
        self.inner.start_open.store(true, Ordering::Release);
    }
}

pub(crate) trait Poll: Process {
    fn poll_tick(
        &mut self,
        ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send
    where
        Self: Sized;
}

pub(crate) struct IntervalDriver<P> {
    interval: Duration,
    _phantom: PhantomData<fn() -> P>,
}

impl<P> IntervalDriver<P> {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            _phantom: PhantomData,
        }
    }
}

impl<P> Driver<P> for IntervalDriver<P>
where
    P: Poll,
{
    type Event = ();

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        let interval = self.interval;
        async move {
            tokio::time::sleep(interval).await;
            Some(())
        }
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        AssertUnwindSafe(state.poll_tick(ctx))
            .catch_unwind()
            .await
            .unwrap_or_else(|_panic| {
                tracing::warn!("durable_stream: poller panic caught — supervisor will restart");
                Err(HandlerError)
            })
    }

    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

pub(crate) struct DurablePollerProcess<T> {
    pub(crate) store: Journal,
    pub(crate) publisher: ProcessProxy<Stream<T>>,
    pub(crate) shared: SharedPollState<T>,
    pub(crate) cursor: SequenceCursor,
}

impl<T> Process for DurablePollerProcess<T> where T: 'static + Send + Sync + Clone {}

impl<T> Poll for DurablePollerProcess<T>
where
    T: 'static + Send + Sync + Clone,
{
    async fn poll_tick(&mut self, _ctx: &mut ProcessContext<Self>) -> Result<(), HandlerError> {
        if !self.shared.is_open() {
            return Ok(());
        }
        poll_once(
            &self.store,
            &self.publisher,
            self.shared.transform(),
            &mut self.cursor,
        )
        .await
    }
}

pub(crate) struct DirectPollerProcess<T, P> {
    pub(crate) store: Journal,
    pub(crate) subscriber: ProcessProxy<P>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) cursor: SequenceCursor,
}

impl<T, P> Process for DirectPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    async fn on_start(&mut self, ctx: &mut ProcessContext<Self>) {
        // When this poller is spawned as a child of the subscriber (the common
        // case via `DurableSubscription::spawn_child`), the parent/child
        // hierarchy already guarantees the poller stops when the subscriber
        // stops — registering an explicit DeathWatch here would cause each
        // subscriber restart to accumulate a stale poller PID in the
        // subscriber's watcher set.
        //
        // Only register the watch when the poller was spawned as a top-level
        // process (subscriber is NOT the parent) so that it still self-stops
        // when the subscriber terminates.
        if ctx.parent() != Some(&self.subscriber.pid()) {
            ctx.watch(self.subscriber.pid()).await;
        }
    }

    async fn on_terminated(&mut self, terminated: Terminated, ctx: &mut ProcessContext<Self>) {
        if terminated.who == self.subscriber.pid() {
            let _ = ctx.stop_self().await;
        }
    }
}

impl<T, P> Poll for DirectPollerProcess<T, P>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    async fn poll_tick(&mut self, _ctx: &mut ProcessContext<Self>) -> Result<(), HandlerError> {
        poll_direct_once(
            &self.store,
            &self.subscriber,
            &self.transform,
            &mut self.cursor,
        )
        .await
    }
}

async fn poll_direct_once<T, P>(
    store: &Journal,
    proxy: &ProcessProxy<P>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) -> Result<(), HandlerError>
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: subscriber direct poll load failed");
            return Ok(());
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: subscriber direct stream error");
                return Ok(());
            }
        };

        let observed = cursor.observed(&loaded);
        match transform(loaded) {
            Some(value) => {
                // Use ask() so the cursor only advances when the subscriber's
                // handler completes successfully.  When the handler returns Err
                // (e.g. a saga whose own append the store refused), ask()
                // propagates that error and the cursor stays at the same
                // position — the upstream message is NOT treated as processed
                // (state consistency).
                match proxy.ask(value).await {
                    Ok(_) => cursor.advance(observed),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "durable_stream: subscriber direct ask failed; \
                             cursor not advanced — upstream message not treated as processed"
                        );
                        return Ok(());
                    }
                }
            }
            None => cursor.advance(observed),
        }
    }
    Ok(())
}

async fn poll_once<T>(
    store: &Journal,
    stream_proxy: &ProcessProxy<Stream<T>>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) -> Result<(), HandlerError>
where
    T: 'static + Send + Sync + Clone,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: event store load failed");
            return Ok(());
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: event store stream error");
                return Ok(());
            }
        };

        let observed = cursor.observed(&loaded);
        match transform(loaded) {
            Some(value) => match stream_proxy.publish(value).await {
                Ok(()) => cursor.advance(observed),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "durable_stream: publish failed, will retry on next poll"
                    );
                    return Ok(());
                }
            },
            None => cursor.advance(observed),
        }
    }
    Ok(())
}
