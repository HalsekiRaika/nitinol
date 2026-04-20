use std::future::Future;
use std::marker::PhantomData;

use crate::process::{Process, ProcessContext, Props, Receive};

/// Trait for processes that subscribe to a `Stream<T>`.
///
/// Implement this instead of `Receive<T>` when you want a simpler interface
/// that receives messages without returning a `Result`. Wrap the implementor
/// with `Props::subscriber` to obtain spawn-ready `Props`.
pub trait Subscriber<T>: 'static + Send + Sync {
    fn recv(&mut self, msg: T, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send;
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
        ctx: &mut ProcessContext,
    ) -> Result<(), std::convert::Infallible> {
        self.inner.recv(msg, ctx).await;
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
