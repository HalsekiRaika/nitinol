use std::future::Future;
use std::pin::Pin;

use crate::error::SpawnError;
use crate::ident::{Pid, ProcessName};
use crate::process::props::{MailboxCapacity, PipeCapacity, Props, StashCapacity, SupervisionStrategy};
use crate::process::spawn::SpawnEnv;
use crate::process::spawnable::{SpawnDispatch, Spawnable};
use crate::process::stream::Stream;
use crate::process::ProcessProxy;

/// Spawn declaration for a `Stream<T>` process registered under a topic.
///
/// `StreamProps<T>` lifts the pre-spec `ProcessSystem::spawn_stream::<T>`
/// hard-coded `Supervision::Resume` (`P7`) into the type system: the
/// supervision strategy is configured at construction and never exposed via
/// a setter. Users tune mailbox / stash / pipe capacity through the same
/// `with_*` API they get on `Props`.
///
/// Attempting to call `with_supervision_strategy` (which does not exist on
/// this type) is a compile error:
///
/// ```compile_fail
/// # use nitinol_runtime::{StreamProps, SupervisionStrategy};
/// # use nitinol_runtime::ident::ProcessName;
/// // compile error: no method `with_supervision_strategy` on `StreamProps<u32>`
/// StreamProps::<u32>::new(ProcessName::new("t"))
///     .with_supervision_strategy(SupervisionStrategy::Stop);
/// ```
pub struct StreamProps<T: 'static + Send + Sync> {
    topic: ProcessName,
    inner: Props<Stream<T>>,
}

impl<T> StreamProps<T>
where
    T: 'static + Send + Sync,
{
    /// Construct a `StreamProps<T>` for the topic name `topic`.
    ///
    /// The internal `Props<Stream<T>>` is pre-configured with
    /// `SupervisionStrategy::Resume`; the strategy field is not reachable
    /// from outside this type.
    pub fn new(topic: ProcessName) -> Self {
        Self {
            topic: topic.clone(),
            inner: Props::new(Stream::<T>::new)
                .with_supervision_strategy(SupervisionStrategy::Resume)
                .with_name(topic),
        }
    }

    pub fn with_mailbox_capacity(mut self, capacity: MailboxCapacity) -> Self {
        self.inner = self.inner.with_mailbox_capacity(capacity);
        self
    }

    pub fn with_stash_capacity(mut self, capacity: StashCapacity) -> Self {
        self.inner = self.inner.with_stash_capacity(capacity);
        self
    }

    pub fn with_pipe_capacity(mut self, capacity: PipeCapacity) -> Self {
        self.inner = self.inner.with_pipe_capacity(capacity);
        self
    }

    pub fn topic(&self) -> &ProcessName {
        &self.topic
    }
}

impl<T> Spawnable for StreamProps<T>
where
    T: 'static + Send + Sync,
{
    type Output = Result<ProcessProxy<Stream<T>>, SpawnError>;
}

impl<T> SpawnDispatch for StreamProps<T>
where
    T: 'static + Send + Sync,
{
    fn spawn_with<'a>(
        self,
        env: &'a SpawnEnv,
    ) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>> {
        Box::pin(async move {
            if env.registry().lookup_by_name(&self.topic).await.is_some() {
                return Err(SpawnError {
                    topic: self.topic.to_string(),
                });
            }
            let proxy = env.spawn(self.inner).await;
            Ok(proxy)
        })
    }

    fn child_pid(output: &Self::Output) -> Option<Pid> {
        output.as_ref().ok().map(|p| p.pid())
    }
}
