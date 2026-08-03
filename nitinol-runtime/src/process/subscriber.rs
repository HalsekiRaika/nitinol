use std::future::Future;
use std::marker::PhantomData;

use crate::error::SendError;
use crate::ident::{Pid, ProcessName};
use crate::process::dead_letter::DeadLetterProxy;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::{Process, ProcessContext, Props, Receive};

use super::wiring;

/// Context passed to `Subscriber<T>::recv`.
///
/// Borrows the underlying [`ProcessContext`]'s identity / wiring fields but
/// hides the internal `SubscriberProcess<S, T>` wrapper from the public API.
/// Generic only over `T` so the subscriber implementor never has to name the
/// adapter type.
pub struct SubscriberContext<'a, T> {
    pid: Pid,
    name: Option<&'a ProcessName>,
    registry: &'a ProcessRegistry,
    sys_tx: &'a tokio::sync::mpsc::Sender<SystemSignal>,
    dead_letter: Option<&'a DeadLetterProxy>,
    _marker: PhantomData<fn(T)>,
}

impl<'a, T> SubscriberContext<'a, T> {
    pub(crate) fn from_process_ctx<S>(ctx: &'a ProcessContext<SubscriberProcess<S, T>>) -> Self
    where
        S: Subscriber<T>,
        T: 'static + Send + Sync,
    {
        Self {
            pid: ctx.pid,
            name: ctx.name.as_ref(),
            registry: &ctx.registry,
            sys_tx: &ctx.sys_tx,
            dead_letter: ctx.dead_letter.as_ref(),
            _marker: PhantomData,
        }
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn name(&self) -> Option<&ProcessName> {
        self.name
    }

    /// Start watching the process at `target_pid` for termination.
    ///
    /// If the target is already absent from the registry, a `WatchRequest` is
    /// routed through `DeadLetterProcess`, which responds with
    /// `Terminated { why: NotFound }`.
    pub async fn watch(&self, target_pid: Pid) {
        wiring::watch(
            self.pid,
            target_pid,
            self.registry,
            self.sys_tx,
            self.dead_letter,
        )
        .await;
    }

    /// Stop watching the process at `target_pid`.
    ///
    /// No-op if the target is no longer in the registry.
    pub async fn unwatch(&self, target_pid: Pid) {
        wiring::unwatch(self.pid, target_pid, self.registry).await;
    }

    /// Send a stop signal to this subscriber process.
    pub async fn stop_self(&self) -> Result<(), SendError> {
        wiring::stop_self(self.sys_tx).await
    }
}

/// Trait for processes that subscribe to a `Stream<T>`.
///
/// Implement this instead of `Receive<T>` when you want a simpler interface
/// that receives messages without returning a `Result`. Wrap the implementor
/// with `Props::subscriber` to get spawn-ready `Props`.
pub trait Subscriber<T>: 'static + Send + Sync {
    fn recv(
        &mut self,
        msg: T,
        ctx: &mut SubscriberContext<'_, T>,
    ) -> impl Future<Output = ()> + Send;
}

/// Internal process that adapts a `Subscriber<T>` into `Process + Receive<T>`.
pub struct SubscriberProcess<S, T> {
    inner: S,
    _marker: PhantomData<T>,
}

impl<S, T> Process for SubscriberProcess<S, T>
where
    S: Subscriber<T>,
    T: 'static + Send + Sync,
{
}

impl<S, T> Receive<T> for SubscriberProcess<S, T>
where
    S: Subscriber<T>,
    T: 'static + Send + Sync,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: T,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let mut sub_ctx = SubscriberContext::<'_, T>::from_process_ctx(ctx);
        self.inner.recv(msg, &mut sub_ctx).await;
        Ok(())
    }
}

impl<S, T> Props<SubscriberProcess<S, T>>
where
    S: Subscriber<T>,
    T: 'static + Send + Sync,
{
    /// Build `Props` for a `Subscriber<T>` implementor.
    ///
    /// The returned `Props` can be passed directly to `ProcessSystem::spawn`.
    /// After spawning, register the resulting proxy with a stream via
    /// `ProcessProxy<Stream<T>>::subscribe(proxy)`.
    pub fn subscriber<F>(factory: F) -> Self
    where
        F: Fn() -> S + 'static + Send + Sync,
    {
        Props::new(move || SubscriberProcess {
            inner: factory(),
            _marker: PhantomData,
        })
    }
}
