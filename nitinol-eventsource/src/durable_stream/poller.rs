use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};
use nitinol_runtime::Stream;
use tokio::sync::oneshot;

use crate::durable_stream::cursor::SequenceCursor;

/// Boxed transform that maps a raw [`LoadedEvent`] into the durable stream's
/// payload type.  Returning `None` skips the event.
pub(crate) type TransformFn<T> =
    Arc<dyn Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static>;

/// Dedicated per-subscriber polling loop.
///
/// Unlike the shared [`run`] loop, this function delivers events directly to a
/// single subscriber via `proxy.tell()` rather than publishing to a shared
/// `Stream<T>` fan-out.  This guarantees that catchup and live events reach
/// the subscriber in ascending sequence order: the event store returns events
/// ordered by sequence, and each delivery is awaited before advancing the
/// cursor.
///
/// The loop runs until the subscriber process is unreachable (dead), at which
/// point polling stops.  Transient errors from the event store are logged and
/// retried on the next poll interval.
pub(crate) async fn run_direct<T, P>(
    store: Arc<dyn EventStore>,
    proxy: ProcessProxy<P>,
    transform: TransformFn<T>,
    mut cursor: SequenceCursor,
    poll_interval: Duration,
) where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    loop {
        let alive = poll_direct_once(&store, &proxy, &transform, &mut cursor).await;
        if !alive {
            return;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// One iteration of the per-subscriber direct polling loop.
///
/// Returns `true` to continue polling, or `false` when the subscriber is gone
/// and the loop should terminate.
async fn poll_direct_once<T, P>(
    store: &Arc<dyn EventStore>,
    proxy: &ProcessProxy<P>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) -> bool
where
    T: 'static + Send + Sync,
    P: Process + Receive<T, Response = ()>,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: subscriber direct poll load failed");
            return true; // transient error — retry on next poll
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: subscriber direct stream error");
                return true; // transient error — retry on next poll
            }
        };

        let observed = cursor.observed(&loaded);
        match (transform)(loaded) {
            Some(value) => {
                if let Err(e) = proxy.tell(value).await {
                    tracing::warn!(
                        error = %e,
                        "durable_stream: subscriber direct tell failed, subscriber dead"
                    );
                    return false; // subscriber gone — stop polling
                }
                cursor.advance(observed);
            }
            // Transform skipped this event. Advance past it to avoid
            // re-loading the same payload on subsequent polls.
            None => cursor.advance(observed),
        }
    }
    true
}

/// Polling loop driven by `tokio::spawn`.
///
/// The loop blocks on `start_rx` before the first iteration.  This guarantees
/// that no events are published — and therefore no cursor advancement occurs —
/// until [`DurableStreamProxy::subscribe`] has registered at least one
/// subscriber.  Without this gate the poller could advance the cursor past
/// catchup events before any subscriber is listening, silently discarding
/// them and violating the at-least-once contract.
///
/// After `start_rx` fires, each iteration fetches events past the cursor,
/// publishes the ones the transform accepts, and sleeps for `poll_interval`.
/// Transient errors are logged but do not abort the loop — only
/// `JoinHandle::abort` (triggered by the proxy's drop-guard) halts polling.
pub(crate) async fn run<T>(
    store: Arc<dyn EventStore>,
    stream_proxy: ProcessProxy<Stream<T>>,
    transform: TransformFn<T>,
    mut cursor: SequenceCursor,
    poll_interval: Duration,
    start_rx: oneshot::Receiver<()>,
) where
    T: 'static + Send + Sync + Clone,
{
    // Wait for the first subscriber to register before polling.
    start_rx.await.ok();
    loop {
        poll_once(&store, &stream_proxy, &transform, &mut cursor).await;
        tokio::time::sleep(poll_interval).await;
    }
}

/// One iteration of the shared polling loop.  Errors short-circuit this iteration but
/// leave the loop intact — the next call will retry from the unchanged cursor.
async fn poll_once<T>(
    store: &Arc<dyn EventStore>,
    stream_proxy: &ProcessProxy<Stream<T>>,
    transform: &TransformFn<T>,
    cursor: &mut SequenceCursor,
) where
    T: 'static + Send + Sync + Clone,
{
    let query = cursor.to_load_query();
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "durable_stream: event store load failed");
            return;
        }
    };

    futures_util::pin_mut!(stream);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = ?e, "durable_stream: event store stream error");
                return;
            }
        };

        let observed = cursor.observed(&loaded);
        match (transform)(loaded) {
            Some(value) => match stream_proxy.publish(value).await {
                Ok(()) => cursor.advance(observed),
                Err(e) => {
                    // At-least-once: leave the cursor untouched so the next
                    // poll re-loads and retries this event.
                    tracing::warn!(
                        error = %e,
                        "durable_stream: publish failed, will retry on next poll"
                    );
                    return;
                }
            },
            // Transform skipped this event.  Advance past it to avoid
            // re-loading the same payload on every poll.
            None => cursor.advance(observed),
        }
    }
}
