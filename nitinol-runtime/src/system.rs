use std::time::Duration;

use crate::error::{SendError, SpawnError};
use crate::ident::{Pid, ProcessName};
use crate::process::run;
use crate::process::{
    AnyProxy, BoxedMessage, DeadLetterProcess, DeadLetterProxy, IdleTimeout, Process, ProcessProxy,
    ProcessRegistry, Props, Receive, Stream, SupervisionStrategy,
};

/// The well-known topic name for the dead-letter stream.
const DEAD_LETTERS_TOPIC: &str = "$dead-letters";

/// The well-known process name for the dead-letter actor.
const DEAD_LETTER_PROCESS_NAME: &str = "$dead-letter";

/// A handle to the system dead-letter stream with a stable, policy-compliant API.
///
/// Wraps `ProcessProxy<Stream<Boxed>>` so that the internal proxy type is not
/// exposed in the public API surface.
pub struct DeadLetterStream(ProcessProxy<Stream<BoxedMessage>>);

impl DeadLetterStream {
    pub fn pid(&self) -> Pid {
        self.0.pid()
    }

    pub async fn subscribe<P>(&self, proxy: ProcessProxy<P>) -> Result<(), SendError>
    where
        P: Process + Receive<BoxedMessage, Response = ()>,
    {
        self.0.subscribe(proxy).await
    }

    pub async fn unsubscribe(&self, pid: Pid) -> Result<(), SendError> {
        self.0.unsubscribe(pid).await
    }
}

pub struct ProcessSystem {
    registry: ProcessRegistry,
    dead_letter_proxy: DeadLetterProxy,
    dead_letter_stream: DeadLetterStream,
    default_idle_timeout: Option<Duration>,
}

impl ProcessSystem {
    pub async fn new() -> Self {
        let registry = ProcessRegistry::new();

        // Spawn the dead-letter stream (persistent — must not be affected by any idle timeout).
        let mut dl_stream_props: Props<Stream<BoxedMessage>> = Props::new(Stream::new);
        dl_stream_props.with_supervision_strategy(SupervisionStrategy::Resume);
        let (dl_stream_process, _dl_stream_idle, dl_stream_supervision) =
            dl_stream_props.into_parts();
        let dl_stream: ProcessProxy<Stream<BoxedMessage>> = run(
            dl_stream_process,
            Some(ProcessName::new(DEAD_LETTERS_TOPIC)),
            registry.clone(),
            None,
            None,
            dl_stream_supervision,
        )
        .await;

        // Spawn the dead-letter actor (persistent — captures the stream proxy and registry).
        let dl_stream_for_producer = dl_stream.clone();
        let registry_for_producer = registry.clone();
        let mut dl_process_props = Props::new(move || {
            DeadLetterProcess::new(dl_stream_for_producer.clone(), registry_for_producer.clone())
        });
        dl_process_props.with_supervision_strategy(SupervisionStrategy::Resume);
        let (dl_process, _dl_process_idle, dl_process_supervision) = dl_process_props.into_parts();
        let dl_process = run(
            dl_process,
            Some(ProcessName::new(DEAD_LETTER_PROCESS_NAME)),
            registry.clone(),
            None,
            None,
            dl_process_supervision,
        )
        .await;

        Self {
            registry,
            dead_letter_proxy: DeadLetterProxy::new(dl_process.user_tx.clone()),
            dead_letter_stream: DeadLetterStream(dl_stream),
            default_idle_timeout: None,
        }
    }

    /// Set a system-wide default idle timeout applied to all user processes whose
    /// `IdleTimeout` is `Inherit`.  Consumes `self` and returns `Self` for chaining.
    pub fn with_default_idle_timeout(mut self, timeout: Duration) -> Self {
        self.default_idle_timeout = Some(timeout);
        self
    }

    /// Returns a reference to the dead-letter stream handle.
    pub fn dead_letter_stream(&self) -> &DeadLetterStream {
        &self.dead_letter_stream
    }

    pub async fn spawn<P: Process>(&self, props: Props<P>) -> ProcessProxy<P> {
        let (process, idle_timeout, supervision) = props.into_parts();
        let timeout = self.resolve_idle_timeout(idle_timeout);
        run(
            process,
            None,
            self.registry.clone(),
            timeout,
            Some(self.dead_letter_proxy.clone()),
            supervision,
        )
        .await
    }

    pub async fn spawn_named<P: Process>(
        &self,
        name: ProcessName,
        props: Props<P>,
    ) -> ProcessProxy<P> {
        let (process, idle_timeout, supervision) = props.into_parts();
        let timeout = self.resolve_idle_timeout(idle_timeout);
        run(
            process,
            Some(name),
            self.registry.clone(),
            timeout,
            Some(self.dead_letter_proxy.clone()),
            supervision,
        )
        .await
    }

    /// Spawn a `Stream<T>` process registered under `topic`.
    ///
    /// Returns an error if a process with the same topic name is already registered.
    pub async fn spawn_stream<T>(
        &self,
        topic: ProcessName,
    ) -> Result<ProcessProxy<Stream<T>>, SpawnError>
    where
        T: 'static + Send + Sync,
    {
        if self.registry.lookup_by_name(&topic).await.is_some() {
            return Err(SpawnError {
                topic: topic.to_string(),
            });
        }
        let mut stream_props: Props<Stream<T>> = Props::new(Stream::new);
        stream_props.with_supervision_strategy(SupervisionStrategy::Resume);
        let (process, idle_timeout, supervision) = stream_props.into_parts();
        let timeout = self.resolve_idle_timeout(idle_timeout);
        let proxy = run(
            process,
            Some(topic),
            self.registry.clone(),
            timeout,
            Some(self.dead_letter_proxy.clone()),
            supervision,
        )
        .await;
        Ok(proxy)
    }

    pub async fn lookup(&self, pid: Pid) -> Option<AnyProxy> {
        self.registry.lookup(pid).await
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        self.registry.lookup_by_name(name).await
    }

    /// Resolve an `IdleTimeout` policy to a concrete `Option<Duration>`.
    fn resolve_idle_timeout(&self, idle_timeout: IdleTimeout) -> Option<Duration> {
        match idle_timeout {
            IdleTimeout::After(dur) => Some(dur),
            IdleTimeout::Persistent => None,
            IdleTimeout::Inherit => self.default_idle_timeout,
        }
    }
}
