use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nitinol_persistence::store::EventStore;
use nitinol_runtime::error::SendError;
use nitinol_runtime::ident::Pid;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};
use nitinol_runtime::Stream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{self, TransformFn};

/// Drop-guard that aborts the polling task when the owning
/// [`DurableStreamProxy`] is dropped.
///
/// Without this guard the polling task would keep running indefinitely after
/// the user discards their handle, holding `Arc<dyn EventStore>` and the
/// underlying `Stream<T>` proxy alive.
pub(crate) struct PollerHandle {
    handle: JoinHandle<()>,
}

impl PollerHandle {
    pub(crate) fn new(handle: JoinHandle<()>) -> Self {
        Self { handle }
    }
}

impl Drop for PollerHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Handle returned by [`DurableStream::spawn`](super::DurableStream::spawn).
///
/// Subscribers attach to the underlying `Stream<T>` topic via
/// [`subscribe`][Self::subscribe] (shared fan-out) or
/// [`subscribe_from`][Self::subscribe_from] (dedicated per-subscriber polling
/// loop that guarantees historical and live events arrive in sequence order).
/// Dropping the proxy aborts the shared polling task and all per-subscriber
/// tasks through the internal drop-guards.
pub struct DurableStreamProxy<T> {
    stream_proxy: ProcessProxy<Stream<T>>,
    // Retained so that `subscribe_from` can spawn per-subscriber polling tasks.
    store: Arc<dyn EventStore>,
    transform: TransformFn<T>,
    poll_interval: Duration,
    // Field-drop order is declaration order, so the shared poller handle is
    // dropped (and its task aborted) before the stream proxy clone is released.
    _poller: PollerHandle,
    // Oneshot sender that gates the shared poller start.  Taken (and fired) on
    // the first `subscribe()` call so the shared poller cannot advance the
    // cursor before at least one shared subscriber is listening.
    // `subscribe_from` does NOT open this gate — its callers use a dedicated
    // per-subscriber poller that is independent of the shared cursor.
    start_tx: Mutex<Option<oneshot::Sender<()>>>,
    // Per-subscriber pollers spawned by `subscribe_from`, keyed by subscriber
    // PID.  Dropping an entry aborts its task; dropping the map aborts all.
    subscriber_pollers: Mutex<HashMap<Pid, PollerHandle>>,
}

impl<T> DurableStreamProxy<T>
where
    T: 'static + Send + Sync,
{
    pub(crate) fn new(
        stream_proxy: ProcessProxy<Stream<T>>,
        poller: PollerHandle,
        start_tx: oneshot::Sender<()>,
        store: Arc<dyn EventStore>,
        transform: TransformFn<T>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            stream_proxy,
            store,
            transform,
            poll_interval,
            _poller: poller,
            start_tx: Mutex::new(Some(start_tx)),
            subscriber_pollers: Mutex::new(HashMap::new()),
        }
    }

    /// Pid of the underlying `Stream<T>` process.
    pub fn pid(&self) -> Pid {
        self.stream_proxy.pid()
    }

    /// Subscribe `proxy` to the durable stream.
    ///
    /// The first call also releases the poller's start gate, ensuring that
    /// catchup polling only begins after at least one subscriber is registered.
    /// This upholds the at-least-once contract: no cursor advancement happens
    /// before someone is listening.
    pub async fn subscribe<P>(&self, proxy: ProcessProxy<P>) -> Result<(), SendError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        self.stream_proxy.subscribe(proxy).await?;
        // Release the poller's start gate on the first subscription.  Taking
        // the sender out of the Mutex and dropping the lock before sending
        // avoids holding the lock across any await point.
        let tx = self.start_tx.lock().unwrap().take();
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
        Ok(())
    }

    /// Subscribe `proxy` to the durable stream from its own checkpoint.
    ///
    /// A dedicated per-subscriber polling loop is spawned that reads events
    /// past `cursor` from the event store and delivers them directly to
    /// `proxy` via `tell`.  The subscriber is **not** registered on the shared
    /// `Stream<T>` fan-out: both historical (catchup) and future (live) events
    /// arrive through the same per-subscriber loop, in ascending sequence
    /// order.  This guarantees that catchup events are never interleaved with
    /// live events, satisfying Switch to live streaming after the catch-up broadcast ends.
    ///
    /// The per-subscriber poller is aborted when this [`DurableStreamProxy`]
    /// is dropped, or when [`unsubscribe`][Self::unsubscribe] is called with
    /// the subscriber's PID.
    ///
    /// Note: this method intentionally does **not** open the shared poller's
    /// start gate.  Only [`subscribe`][Self::subscribe] opens it, ensuring the
    /// shared `Stream<T>` cursor cannot advance while there are no shared
    /// subscribers.  Mixing `subscribe_from`-only callers with a later
    /// `subscribe` call is safe: the shared poller starts from its initial
    /// cursor when `subscribe` is eventually called.
    pub async fn subscribe_from<P>(
        &self,
        proxy: ProcessProxy<P>,
        cursor: SequenceCursor,
    ) -> Result<(), SendError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        let pid = proxy.pid();
        let store = Arc::clone(&self.store);
        let transform = Arc::clone(&self.transform);
        let poll_interval = self.poll_interval;

        // Dedicated poller: reads from event store in sequence order and
        // delivers directly to this subscriber.  Historical and live events
        // both flow through this single path, so ordering is guaranteed.
        let handle = tokio::spawn(async move {
            poller::run_direct(store, proxy, transform, cursor, poll_interval).await;
        });

        self.subscriber_pollers
            .lock()
            .unwrap()
            .insert(pid, PollerHandle::new(handle));

        Ok(())
    }

    /// Remove the subscriber identified by `pid`.
    ///
    /// For subscribers registered via [`subscribe`][Self::subscribe], this
    /// removes them from the shared `Stream<T>` fan-out.  For subscribers
    /// registered via [`subscribe_from`][Self::subscribe_from], this aborts
    /// their dedicated polling task.  If `pid` is not currently subscribed,
    /// this is a no-op.
    pub async fn unsubscribe(&self, pid: Pid) -> Result<(), SendError> {
        // Abort per-subscriber poller first so it is always cleaned up even
        // when the shared-stream send fails below.
        self.subscriber_pollers.lock().unwrap().remove(&pid);
        // Remove from shared fan-out and propagate any send error to the caller.
        self.stream_proxy.unsubscribe(pid).await
    }
}
