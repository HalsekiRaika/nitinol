use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::store::EventStore;
use nitinol_runtime::error::SendError;
use nitinol_runtime::ident::{Pid, ProcessName};
use nitinol_runtime::process::{Process, ProcessProxy, Props, Receive, SupervisionStrategy};
use nitinol_runtime::{ProcessSystem, Stream};

use crate::durable_stream::cursor::SequenceCursor;
use crate::durable_stream::poller::{
    DirectPollerProcess, DurablePollerProcess, IntervalDriver, TransformFn,
};

const POLLER_RESTART_MAX_RETRIES: u32 = 5;
const POLLER_RESTART_WITHIN: Duration = Duration::from_secs(60);

pub(crate) fn shared_poller_name(stream_pid: Pid) -> ProcessName {
    ProcessName::new(format!("durable-poller-{stream_pid}"))
}

pub(crate) fn direct_poller_name(subscriber_pid: Pid) -> ProcessName {
    ProcessName::new(format!("direct-poller-{subscriber_pid}"))
}

pub struct DurableStreamProxy<T> {
    stream_proxy: ProcessProxy<Stream<T>>,
    store: Arc<dyn EventStore>,
    transform: TransformFn<T>,
    poll_interval: Duration,
    shared_poller: ProcessProxy<DurablePollerProcess<T>>,
    start_open: Arc<AtomicBool>,
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

        let store = Arc::clone(&self.store);
        let transform = Arc::clone(&self.transform);
        let subscriber = proxy.clone();
        let initial_cursor = cursor;
        let owner_pid = self.shared_poller.pid();

        let mut props = Props::new(move || DirectPollerProcess {
            store: Arc::clone(&store),
            subscriber: subscriber.clone(),
            transform: Arc::clone(&transform),
            cursor: initial_cursor.clone(),
            owner_pid,
        });
        props.with_supervision_strategy(SupervisionStrategy::Restart {
            max_retries: POLLER_RESTART_MAX_RETRIES,
            within: POLLER_RESTART_WITHIN,
        });

        let driver = IntervalDriver::<DirectPollerProcess<T, P>>::new(self.poll_interval);
        system
            .spawn_named_with_driver(direct_poller_name(pid), props, driver)
            .await;

        Ok(())
    }

    pub async fn unsubscribe(&self, system: &ProcessSystem, pid: Pid) -> Result<(), SendError> {
        if let Some(proxy) = system.lookup_by_name(&direct_poller_name(pid)).await {
            let _ = proxy.stop().await;
        }
        self.stream_proxy.unsubscribe(pid).await
    }
}

impl<T> Drop for DurableStreamProxy<T> {
    fn drop(&mut self) {
        self.shared_poller.signal_stop_nonblocking();
    }
}
