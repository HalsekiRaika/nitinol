use crate::error::BoxError;
use crate::ident::ProcessName;
use crate::process::run;
use crate::process::{AnyProxy, Boxed, DeadLetterProcess, DeadLetterProxy, Process, ProcessProxy, ProcessRegistry, Props, Stream};

/// The well-known topic name for the dead-letter stream.
const DEAD_LETTERS_TOPIC: &str = "$dead-letters";

/// The well-known process name for the dead-letter actor.
const DEAD_LETTER_ACTOR_NAME: &str = "$dead-letter";

pub struct ProcessSystem {
    registry: ProcessRegistry,
    dead_letter_ref: DeadLetterProxy,
    dead_letter_stream: ProcessProxy<Stream<Boxed>>,
}

impl ProcessSystem {
    pub async fn new() -> Self {
        let registry = ProcessRegistry::new();

        // Spawn the dead-letter stream (no dead-letter routing for itself).
        let dl_stream_process = Stream::<Boxed>::new();
        let dl_stream: ProcessProxy<Stream<Boxed>> = run(
            dl_stream_process,
            Some(ProcessName::new(DEAD_LETTERS_TOPIC)),
            registry.clone(),
            None,
            None,
            None,
        )
        .await;

        // Spawn the dead-letter actor (captures the stream proxy and registry).
        let dl_actor_process = DeadLetterProcess::new(dl_stream.clone(), registry.clone());
        let dl_actor = run(
            dl_actor_process,
            Some(ProcessName::new(DEAD_LETTER_ACTOR_NAME)),
            registry.clone(),
            None,
            None,
            None,
        )
        .await;

        let dead_letter_ref = DeadLetterProxy::new(dl_actor.user_tx.clone());

        Self {
            registry,
            dead_letter_ref,
            dead_letter_stream: dl_stream,
        }
    }

    /// Returns a reference to the dead-letter stream proxy.
    pub fn dead_letter_stream(&self) -> &ProcessProxy<Stream<Boxed>> {
        &self.dead_letter_stream
    }

    pub async fn spawn<P: Process>(&self, props: Props<P>) -> ProcessProxy<P> {
        let (process, supervision) = props.into_parts();
        run(
            process,
            None,
            self.registry.clone(),
            None,
            Some(self.dead_letter_ref.clone()),
            Some(supervision),
        )
        .await
    }

    pub async fn spawn_named<P: Process>(
        &self,
        name: ProcessName,
        props: Props<P>,
    ) -> ProcessProxy<P> {
        let (process, supervision) = props.into_parts();
        run(
            process,
            Some(name),
            self.registry.clone(),
            None,
            Some(self.dead_letter_ref.clone()),
            Some(supervision),
        )
        .await
    }

    /// Spawn a `Stream<T>` process registered under `topic`.
    ///
    /// Returns an error if a process with the same topic name is already registered.
    pub async fn spawn_stream<T: 'static + Send + Sync>(
        &self,
        topic: ProcessName,
    ) -> Result<ProcessProxy<Stream<T>>, BoxError> {
        if self.registry.lookup_by_name(&topic).await.is_some() {
            return Err(format!("stream topic '{}' already registered", topic).into());
        }
        let process = Stream::new();
        let proxy = run(
            process,
            Some(topic),
            self.registry.clone(),
            None,
            Some(self.dead_letter_ref.clone()),
            None,
        )
        .await;
        Ok(proxy)
    }

    pub async fn lookup(&self, pid: crate::ident::Pid) -> Option<AnyProxy> {
        self.registry.lookup(pid).await
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        self.registry.lookup_by_name(name).await
    }
}
