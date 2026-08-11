use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::EventStore;
use nitinol_persistence::LoadedEvent;
use nitinol_runtime::error::SendError;
use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{
    Process, ProcessContext, ProcessProxy, Receive, SupervisionStrategy,
};
use nitinol_runtime::{ProcessSystem, Props, Stream};

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{
    DirectPollerProcess, DurablePollerProcess, IntervalDriver, TransformFn,
};

const POLLER_RESTART_MAX_RETRIES: u32 = 5;
const POLLER_RESTART_WITHIN: Duration = Duration::from_secs(60);

/// Minimal configuration needed to spawn a [`DirectPollerProcess`] as a
/// runtime child of any subscriber process via [`Self::spawn_child`].
///
/// Use this instead of holding a full [`DurableStreamProxy`] when the
/// subscriber only needs its own direct polling path and does not use the
/// shared [`Stream<T>`] fan-out channel.  Holding a `DurableStreamProxy` as a
/// process field keeps the shared poller alive for the process's lifetime —
/// `DurableSubscription` avoids that by carrying only the ingredients for a
/// direct poller, with no reference to the shared poller.
pub struct DurableSubscription<T> {
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) transform: TransformFn<T>,
    pub(crate) poll_interval: Duration,
}

impl<T> Clone for DurableSubscription<T> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            transform: Arc::clone(&self.transform),
            poll_interval: self.poll_interval,
        }
    }
}

impl<T: 'static + Send + Sync> DurableSubscription<T> {
    /// Create a config with the default polling cadence (250 ms).
    ///
    /// Use [`Self::with_poll_interval`] to override the interval.
    pub fn new<F>(store: Arc<dyn EventStore>, transform: F) -> Self
    where
        F: Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static,
    {
        Self {
            store,
            transform: Arc::new(transform),
            poll_interval: super::DEFAULT_POLL_INTERVAL,
        }
    }

    /// Override the polling cadence.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Build a fully-wired `Props` for a [`DirectPollerProcess`] — supervision
    /// strategy, custom interval driver, and process construction in one
    /// place. Both [`Self::spawn_child`] and
    /// [`DurableStreamProxy::subscribe_from`] delegate here so that changes
    /// to any of these values are made exactly once.
    fn make_direct_poller_props<S>(
        &self,
        subscriber: ProcessProxy<S>,
        cursor: SequenceCursor,
        name: Option<ProcessName>,
    ) -> Props![DirectPollerProcess<T, S>; IntervalDriver]
    where
        S: Process + Receive<T, Response = ()>,
    {
        let store = Arc::clone(&self.store);
        let transform = Arc::clone(&self.transform);
        let initial_cursor = cursor;
        let driver = IntervalDriver::<DirectPollerProcess<T, S>>::new(self.poll_interval);
        let restart_strategy =
            SupervisionStrategy::restart(POLLER_RESTART_MAX_RETRIES, POLLER_RESTART_WITHIN)
                .expect("POLLER_RESTART_WITHIN is a positive duration constant");
        let mut props = Props::new(move || DirectPollerProcess {
            store: Arc::clone(&store),
            subscriber: subscriber.clone(),
            transform: Arc::clone(&transform),
            cursor: initial_cursor.clone(),
        })
        .with_supervision_strategy(restart_strategy)
        .with_driver(driver);
        if let Some(n) = name {
            props = props.with_name(n);
        }
        props
    }

    /// Spawn a [`DirectPollerProcess`] as a runtime **child** of `ctx`'s
    /// process.
    ///
    /// The poller's lifetime is tied to the calling process: when the calling
    /// process stops for any reason the runtime cascade-stops the child poller
    /// automatically.
    pub async fn spawn_child<S>(&self, ctx: &mut ProcessContext<S>, cursor: SequenceCursor)
    where
        S: Process + Receive<T, Response = ()>,
    {
        let subscriber = ctx.self_proxy().clone();
        let props = self.make_direct_poller_props(subscriber, cursor, None);
        ctx.spawn_child(props).await;
    }

    /// Spawn a [`DirectPollerProcess`] as a runtime **child** of `ctx`'s process,
    /// forwarding events to an external `subscriber` process.
    ///
    /// Unlike [`Self::spawn_child`] (where the subscriber IS `ctx`'s process), this
    /// method separates the parent (lifecycle owner) from the subscriber (event
    /// receiver).  The poller's lifetime is tied to `ctx`'s process: when that
    /// process stops the runtime cascade-stops the child poller automatically.
    ///
    /// Use this when a parent process (e.g. a saga) owns the subscription
    /// lifetime while an external subscriber process receives the events.
    ///
    /// `name` must be unique within `ctx`'s children.
    pub async fn spawn_child_for<Parent, Sub>(
        &self,
        ctx: &mut ProcessContext<Parent>,
        subscriber: ProcessProxy<Sub>,
        cursor: SequenceCursor,
        name: ProcessName,
    ) where
        Parent: Process,
        Sub: Process + Receive<T, Response = ()>,
    {
        let props = self.make_direct_poller_props(subscriber, cursor, Some(name));
        ctx.spawn_child(props).await;
    }

    /// Spawn a [`DirectPollerProcess`] via a [`ProcessSystem`] reference.
    ///
    /// Unlike [`Self::spawn_child`] this does not require being inside the
    /// subscriber's [`ProcessContext`], so it can be called from external
    /// wiring code that only holds a [`ProcessProxy<S>`].
    ///
    /// `poller_name` is caller-supplied so that distinct subscriptions from the
    /// same subscriber process to different source streams get unique names and
    /// coexist as separate pollers.  If a direct poller under `poller_name` is
    /// already registered (e.g., from a previous incarnation with the same
    /// subscription key), it is stopped and replaced so the subscriber
    /// re-catchups from `cursor`.  Pollers under different names are unaffected.
    pub async fn spawn_from<S>(
        &self,
        system: &ProcessSystem,
        subscriber: ProcessProxy<S>,
        cursor: SequenceCursor,
        poller_name: ProcessName,
    ) where
        S: Process + Receive<T, Response = ()>,
    {
        if let Some(existing) = system.lookup_by_name(&poller_name).await {
            let _ = existing.stop().await;
        }
        let props = self.make_direct_poller_props(subscriber, cursor, Some(poller_name));
        system.spawn(props).await;
    }
}

pub(crate) fn shared_poller_name(stream_pid: Pid) -> ProcessName {
    ProcessName::new(format!("durable-poller-{stream_pid}"))
}

pub(crate) fn direct_poller_name(subscriber_pid: Pid) -> ProcessName {
    ProcessName::new(format!("direct-poller-{subscriber_pid}"))
}

/// Handle to a running [`crate::DurableStream`].
///
/// The `Drop` impl signals the shared poller to stop when the handle is
/// released.
pub struct DurableStreamProxy<T> {
    stream_proxy: ProcessProxy<Stream<T>>,
    store: Arc<dyn EventStore>,
    transform: TransformFn<T>,
    poll_interval: Duration,
    shared_poller: ProcessProxy<DurablePollerProcess<T>>,
    start_open: Arc<AtomicBool>,
}

impl<T> Drop for DurableStreamProxy<T> {
    fn drop(&mut self) {
        self.shared_poller.signal_stop_nonblocking();
    }
}

impl<T> DurableStreamProxy<T>
where
    T: 'static + Send + Sync + Clone,
{
    pub(crate) fn new(
        stream_proxy: ProcessProxy<Stream<T>>,
        shared_poller: ProcessProxy<DurablePollerProcess<T>>,
        start_open: Arc<AtomicBool>,
        store: Arc<dyn EventStore>,
        transform: TransformFn<T>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            stream_proxy,
            store,
            transform,
            poll_interval,
            shared_poller,
            start_open,
        }
    }

    pub fn pid(&self) -> Pid {
        self.stream_proxy.pid()
    }

    pub async fn subscribe<P>(&self, proxy: ProcessProxy<P>) -> Result<(), SendError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        self.stream_proxy.subscribe(proxy).await?;
        self.start_open.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn subscribe_from<P>(
        &self,
        system: &ProcessSystem,
        proxy: ProcessProxy<P>,
        cursor: SequenceCursor,
    ) -> Result<(), SendError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        let pid = proxy.pid();

        if let Some(existing) = system.lookup_by_name(&direct_poller_name(pid)).await {
            let _ = existing.stop().await;
        }

        let config = DurableSubscription {
            store: Arc::clone(&self.store),
            transform: Arc::clone(&self.transform),
            poll_interval: self.poll_interval,
        };
        let props = config.make_direct_poller_props(proxy, cursor, Some(direct_poller_name(pid)));
        system.spawn(props).await;

        Ok(())
    }

    pub async fn unsubscribe(&self, system: &ProcessSystem, pid: Pid) -> Result<(), SendError> {
        if let Some(proxy) = system.lookup_by_name(&direct_poller_name(pid)).await {
            let _ = proxy.stop().await;
        }
        self.stream_proxy.unsubscribe(pid).await
    }
}
