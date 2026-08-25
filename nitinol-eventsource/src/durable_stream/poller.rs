use std::future::Future;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{
    Driver, Process, ProcessContext, ProcessProxy, Receive, Terminated,
};
use nitinol_runtime::Stream;

use crate::durable_stream::cursor::SequenceCursor;

pub(crate) type TransformFn<T> = Arc<dyn Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static>;

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
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) publisher: ProcessProxy<Stream<T>>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) cursor: SequenceCursor,
    pub(crate) start_open: Arc<AtomicBool>,
}

impl<T> Process for DurablePollerProcess<T> where T: 'static + Send + Sync + Clone {}

impl<T> Poll for DurablePollerProcess<T>
where
    T: 'static + Send + Sync + Clone,
{
    async fn poll_tick(&mut self, _ctx: &mut ProcessContext<Self>) -> Result<(), HandlerError> {
        if !self.start_open.load(Ordering::Acquire) {
            return Ok(());
        }
        poll_once(
            &self.store,
            &self.publisher,
            &self.transform,
            &mut self.cursor,
        )
        .await
    }
}

pub(crate) struct DirectPollerProcess<T, P> {
    pub(crate) store: Arc<dyn EventStore>,
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
    store: &Arc<dyn EventStore>,
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
                // (e.g. Saga DLQ append failure), ask() propagates that error
                // and the cursor stays at the same position — the upstream
                // message is NOT treated as processed (state consistency).
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
    store: &Arc<dyn EventStore>,
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
