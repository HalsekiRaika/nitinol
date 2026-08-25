use std::sync::Arc;
use std::time::Duration;

use crate::error::SendError;
use crate::ident::{Pid, ProcessName};
use crate::process::spawn::SpawnEnv;
use crate::process::spawnable::SpawnDispatch;
use crate::process::{
    AnyProxy, BoxedMessage, DeadLetterProcess, DeadLetterProxy, MailboxCapacity, PipeCapacity,
    Process, ProcessProxy, ProcessRegistry, Props, Receive, StashCapacity, Stream,
    SupervisionStrategy,
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

/// Configuration for a [`ProcessSystem`], and the only place it can be set.
///
/// Obtained from [`ProcessSystem::builder`].  Each `with_default_*` axis names
/// the value a `Props` field left at `Inherit` decays to at spawn time;
/// [`build`][Self::build] then performs the construction and hands back a system
/// carrying them.
pub struct ProcessSystemBuilder {
    default_idle_timeout: Option<Duration>,
    default_mailbox_capacity: MailboxCapacity,
    default_stash_capacity: StashCapacity,
    default_pipe_capacity: PipeCapacity,
}

impl ProcessSystemBuilder {
    /// Set a system-wide default idle timeout applied to all user processes whose
    /// `IdleTimeout` is `Inherit`.  Consumes `self` and returns `Self` for chaining.
    pub fn with_default_idle_timeout(mut self, timeout: Duration) -> Self {
        self.default_idle_timeout = Some(timeout);
        self
    }

    /// Set the system-wide default mailbox capacity.
    pub fn with_default_mailbox_capacity(mut self, capacity: MailboxCapacity) -> Self {
        self.default_mailbox_capacity = capacity;
        self
    }

    /// Set the system-wide default stash capacity.
    pub fn with_default_stash_capacity(mut self, capacity: StashCapacity) -> Self {
        self.default_stash_capacity = capacity;
        self
    }

    /// Set the system-wide default pipe capacity.
    pub fn with_default_pipe_capacity(mut self, capacity: PipeCapacity) -> Self {
        self.default_pipe_capacity = capacity;
        self
    }

    /// Build the system: start the dead-letter infrastructure and settle the
    /// configured defaults into the handle that is returned.
    ///
    /// This is construction, not just configuration — the dead-letter stream and
    /// actor are running by the time it returns, so the first spawn already has
    /// somewhere to route to.
    pub async fn build(self) -> ProcessSystem {
        let registry = ProcessRegistry::new();

        // Bootstrap env for the dead-letter stream + actor. The configured
        // defaults are deliberately withheld from it: the dead-letter
        // infrastructure is persistent, and a system that reaped its own
        // dead-letter stream on the caller's idle timeout would silently lose
        // every dead letter from then on.
        let bootstrap_env = SpawnEnv::detached(registry.clone(), None);

        // Spawn the dead-letter stream (persistent — must not be affected by any idle timeout).
        let dl_stream_props: Props<Stream<BoxedMessage>> = Props::new(Stream::new)
            .with_supervision_strategy(SupervisionStrategy::Resume)
            .with_name(ProcessName::new(DEAD_LETTERS_TOPIC));
        let dl_stream: ProcessProxy<Stream<BoxedMessage>> =
            bootstrap_env.spawn(dl_stream_props).await;

        // Spawn the dead-letter actor (persistent — captures the stream proxy and registry).
        let dl_stream_for_producer = dl_stream.clone();
        let registry_for_producer = registry.clone();
        let dl_process_props = Props::new(move || {
            DeadLetterProcess::new(
                dl_stream_for_producer.clone(),
                registry_for_producer.clone(),
            )
        })
        .with_supervision_strategy(SupervisionStrategy::Resume)
        .with_name(ProcessName::new(DEAD_LETTER_PROCESS_NAME));
        let dl_process = bootstrap_env.spawn(dl_process_props).await;

        ProcessSystem {
            inner: Arc::new(SystemInner {
                registry,
                dead_letter_proxy: DeadLetterProxy::new(dl_process.user_tx.clone()),
                dead_letter_stream: DeadLetterStream(dl_stream),
                default_idle_timeout: self.default_idle_timeout,
                default_mailbox_capacity: self.default_mailbox_capacity,
                default_stash_capacity: self.default_stash_capacity,
                default_pipe_capacity: self.default_pipe_capacity,
            }),
        }
    }
}

/// One process system, addressed through a cheaply cloned handle.
///
/// Cloning a `ProcessSystem` names the same instance rather than creating a
/// second one: every clone observes the same process registry, the same
/// dead-letter stream, and the same defaults.  Keeping the instance single is
/// this type's job, so callers share it by cloning rather than by wrapping it in
/// an `Arc`, and the entry points below stay by-reference — whether to clone is
/// the caller's decision, not one this API makes for them.
///
/// Configuration is fixed before any handle exists.  The `with_default_*` axes
/// live on [`ProcessSystemBuilder`], and
/// [`build`][ProcessSystemBuilder::build] settles them; a built system has no
/// way to be reconfigured, so no clone can observe defaults that differ from
/// another's.
#[derive(Clone)]
pub struct ProcessSystem {
    inner: Arc<SystemInner>,
}

/// The instance every [`ProcessSystem`] handle shares.
struct SystemInner {
    registry: ProcessRegistry,
    dead_letter_proxy: DeadLetterProxy,
    dead_letter_stream: DeadLetterStream,
    default_idle_timeout: Option<Duration>,
    default_mailbox_capacity: MailboxCapacity,
    default_stash_capacity: StashCapacity,
    default_pipe_capacity: PipeCapacity,
}

impl ProcessSystem {
    /// Build a system with the default configuration.
    ///
    /// Shorthand for `ProcessSystem::builder().build().await`; reach for
    /// [`builder`][Self::builder] when any of the system-wide defaults has to be
    /// set.
    pub async fn new() -> Self {
        Self::builder().build().await
    }

    /// Begin configuring a system.
    ///
    /// # Configuration is fixed by `build()`
    ///
    /// The `with_default_*` axes exist on the builder alone.  Reconfiguring a
    /// system after it has been built — and after clones of it may already be in
    /// flight — is a **compile error** rather than a change some handles would
    /// see and others would not:
    ///
    /// ```compile_fail
    /// # use std::time::Duration;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// // compile error: `with_default_idle_timeout` lives on `ProcessSystemBuilder`
    /// let _ = system.with_default_idle_timeout(Duration::from_secs(30));
    /// # }
    /// ```
    ///
    /// The same default, set where it belongs, compiles:
    ///
    /// ```rust,no_run
    /// # use std::time::Duration;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// let system = ProcessSystem::builder()
    ///     .with_default_idle_timeout(Duration::from_secs(30))
    ///     .build()
    ///     .await;
    /// # let _ = system;
    /// # }
    /// ```
    pub fn builder() -> ProcessSystemBuilder {
        ProcessSystemBuilder {
            default_idle_timeout: None,
            default_mailbox_capacity: MailboxCapacity::Inherit,
            default_stash_capacity: StashCapacity::Inherit,
            default_pipe_capacity: PipeCapacity::Inherit,
        }
    }

    /// Returns a reference to the dead-letter stream handle.
    pub fn dead_letter_stream(&self) -> &DeadLetterStream {
        &self.inner.dead_letter_stream
    }

    /// Spawn `spawnable`. The single unified entry — `Props<P>` returns a
    /// `ProcessProxy<P>`; `StreamProps<T>` returns `Result<ProcessProxy<Stream<T>>, SpawnError>`.
    ///
    /// The legacy `spawn_named`, `spawn_with_driver`, and `spawn_stream` entry
    /// points have been removed. Attempting to call any of them is a compile error:
    ///
    /// ```compile_fail
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// // compile error: `spawn_named` was removed — use `props.with_name(name)` instead
    /// system.spawn_named("x", todo!()).await;
    /// # }
    /// ```
    ///
    /// ```compile_fail
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// // compile error: `spawn_with_driver` was removed — use `props.with_driver(driver)` instead
    /// system.spawn_with_driver(todo!(), todo!()).await;
    /// # }
    /// ```
    ///
    /// ```compile_fail
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// // compile error: `spawn_stream` was removed — use `system.spawn(StreamProps::<T>::new(topic))` instead
    /// system.spawn_stream::<u32>(todo!()).await;
    /// # }
    /// ```
    ///
    /// The internal dispatch trait `Spawnable` is not part of the public API;
    /// only `Props` and `StreamProps` are accepted:
    ///
    /// ```compile_fail
    /// // compile error: `Spawnable` is not exported — internal dispatch stays crate-private
    /// use nitinol_runtime::process::Spawnable;
    /// ```
    #[allow(private_bounds)]
    pub async fn spawn<S: SpawnDispatch>(&self, spawnable: S) -> S::Output {
        let env = SpawnEnv::top_level(
            self.inner.registry.clone(),
            self.inner.dead_letter_proxy.clone(),
            self.inner.default_idle_timeout,
            self.inner.default_mailbox_capacity,
            self.inner.default_stash_capacity,
            self.inner.default_pipe_capacity,
        );
        spawnable.spawn_with(&env).await
    }

    pub async fn lookup(&self, pid: Pid) -> Option<AnyProxy> {
        self.inner.registry.lookup(pid).await
    }

    pub async fn lookup_by_name(&self, name: &ProcessName) -> Option<AnyProxy> {
        self.inner.registry.lookup_by_name(name).await
    }
}
