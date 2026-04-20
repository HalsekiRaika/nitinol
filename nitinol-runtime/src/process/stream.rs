use std::collections::HashMap;
use std::future::Future;

use async_trait::async_trait;

use crate::error::BoxError;
use crate::ident::Pid;
use crate::process::message::{Boxed, Message};
use crate::process::watch::Terminated;
use crate::process::{Process, ProcessContext, ProcessProxy, Receive};

// -- Internal dispatch abstraction ------------------------------------------

/// Object-safe trait for delivering a message to a subscriber process.
///
/// `#[async_trait]` is required so that `Box<dyn Dispatcher<T>>` is object-safe.
/// All implementors must be `'static + Send + Sync`.
#[async_trait]
pub(crate) trait Dispatcher<T>: 'static + Send + Sync {
    fn pid(&self) -> Pid;
    async fn dispatch(&self, msg: T) -> Result<(), BoxError>;
}

#[async_trait]
impl<P, T> Dispatcher<T> for ProcessProxy<P>
where
    P: Process + Receive<T, Response = ()>,
    T: 'static + Send + Sync,
{
    fn pid(&self) -> Pid {
        self.pid
    }

    async fn dispatch(&self, msg: T) -> Result<(), BoxError> {
        self.tell(msg).await
    }
}

// -- Internal protocol messages ---------------------------------------------

pub(crate) struct PublishMsg<T>(pub T);
pub(crate) struct SubscribeMsg<T>(pub Box<dyn Dispatcher<T>>);
pub(crate) struct UnsubscribeMsg(pub Pid);

// -- Stream process ---------------------------------------------------------

/// A built-in pub/sub process.
///
/// Topic-scoped: each unique topic name maps to exactly one `Stream` instance
/// in a `ProcessSystem`. Subscribers are stored as type-erased `Dispatcher<T>`,
/// keyed by PID for O(1) removal on termination or unsubscribe.
pub struct Stream<T = Boxed> {
    subscribers: HashMap<Pid, Box<dyn Dispatcher<T>>>,
}

impl<T: 'static + Send + Sync> Stream<T> {
    pub(crate) fn new() -> Self {
        Self {
            subscribers: HashMap::new(),
        }
    }
}

impl<T: 'static + Send + Sync> Process for Stream<T> {
    /// Remove the terminated subscriber from the list.
    ///
    /// No unwatch needed: the target is already stopped.
    fn on_terminated(
        &mut self,
        terminated: Terminated,
        _ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        self.subscribers.remove(&terminated.who);
        async {}
    }
}

/// Broadcast a published message to every subscriber.
///
/// Delivery failure for one subscriber does not abort delivery to the rest.
impl<T: 'static + Send + Sync + Clone> Receive<PublishMsg<T>> for Stream<T> {
    type Response = ();
    async fn recv(
        &mut self,
        msg: PublishMsg<T>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        for dispatcher in self.subscribers.values() {
            let _ = dispatcher.dispatch(msg.0.clone()).await;
        }
        Ok(())
    }
}

/// Register a new subscriber dispatcher and start watching it for termination.
impl<T: 'static + Send + Sync> Receive<SubscribeMsg<T>> for Stream<T> {
    type Response = ();
    async fn recv(
        &mut self,
        msg: SubscribeMsg<T>,
        ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        let pid = msg.0.pid();
        self.subscribers.insert(pid, msg.0);
        ctx.watch(pid).await;
        Ok(())
    }
}

/// Remove a subscriber by PID and stop watching it.
impl<T: 'static + Send + Sync> Receive<UnsubscribeMsg> for Stream<T> {
    type Response = ();
    async fn recv(
        &mut self,
        msg: UnsubscribeMsg,
        ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        if self.subscribers.remove(&msg.0).is_some() {
            ctx.unwatch(msg.0).await;
        }
        Ok(())
    }
}

// -- ProcessProxy extensions ------------------------------------------------

impl ProcessProxy<Stream<Boxed>> {
    /// Publish any `Message` value to all subscribers of this stream.
    ///
    /// The value is type-erased into `Boxed` so every subscriber receives
    /// the same zero-copy `Arc` clone.
    pub async fn publish<M: Message>(&self, msg: M) -> Result<(), BoxError> {
        self.tell(PublishMsg(Boxed::new(msg))).await
    }
}

impl<T: 'static + Send + Sync> ProcessProxy<Stream<T>> {
    /// Register `proxy` as a subscriber to this stream.
    ///
    /// The proxy's process must implement `Receive<T, Response = ()>`.
    pub async fn subscribe<P>(&self, proxy: ProcessProxy<P>) -> Result<(), BoxError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        self.tell(SubscribeMsg(Box::new(proxy))).await
    }

    /// Remove the subscriber identified by `pid` from this stream.
    ///
    /// No-op if `pid` is not currently subscribed.
    pub async fn unsubscribe(&self, pid: Pid) -> Result<(), BoxError> {
        self.tell(UnsubscribeMsg(pid)).await
    }
}
