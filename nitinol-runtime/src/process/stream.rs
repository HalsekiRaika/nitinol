use async_trait::async_trait;

use crate::error::BoxError;
use crate::process::message::{Boxed, Message};
use crate::process::{Process, ProcessContext, ProcessProxy, Receive};

// -- Internal dispatch abstraction ------------------------------------------

/// Object-safe trait for delivering a message to a subscriber process.
///
/// `#[async_trait]` is required so that `Box<dyn Dispatcher<T>>` is object-safe.
/// All implementors must be `'static + Send + Sync`.
#[async_trait]
pub(crate) trait Dispatcher<T>: 'static + Send + Sync {
    async fn dispatch(&self, msg: T) -> Result<(), BoxError>;
}

#[async_trait]
impl<P, T> Dispatcher<T> for ProcessProxy<P>
where
    P: Process + Receive<T, Response = ()>,
    T: 'static + Send + Sync,
{
    async fn dispatch(&self, msg: T) -> Result<(), BoxError> {
        self.tell(msg).await
    }
}

// -- Internal protocol messages ---------------------------------------------

pub(crate) struct PublishMsg<T>(pub T);
pub(crate) struct SubscribeMsg<T>(pub Box<dyn Dispatcher<T>>);

// -- Stream process ---------------------------------------------------------

/// A built-in pub/sub process.
///
/// Topic-scoped: each unique topic name maps to exactly one `Stream` instance
/// in a `ProcessSystem`. Subscribers are stored as type-erased `Dispatcher<T>`.
pub struct Stream<T = Boxed> {
    subscribers: Vec<Box<dyn Dispatcher<T>>>,
}

impl<T: 'static + Send + Sync> Stream<T> {
    pub(crate) fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }
}

impl<T: 'static + Send + Sync> Process for Stream<T> {}

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
        for dispatcher in &self.subscribers {
            let _ = dispatcher.dispatch(msg.0.clone()).await;
        }
        Ok(())
    }
}

/// Register a new subscriber dispatcher.
impl<T: 'static + Send + Sync> Receive<SubscribeMsg<T>> for Stream<T> {
    type Response = ();
    async fn recv(
        &mut self,
        msg: SubscribeMsg<T>,
        _ctx: &mut ProcessContext,
    ) -> Result<(), BoxError> {
        self.subscribers.push(msg.0);
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
}
