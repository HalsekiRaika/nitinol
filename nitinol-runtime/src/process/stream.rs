use std::any::TypeId;
use std::collections::HashMap;
use std::future::Future;

use async_trait::async_trait;

use crate::error::SendError;
use crate::ident::Pid;
use crate::process::dead_letter::{suppress_log, DeadLetterEnvelope};
use crate::process::message::{BoxedMessage, Message};
use crate::process::task::{TellTask, UserTask};
use crate::process::watch::Terminated;
use crate::process::{Process, ProcessContext, ProcessProxy, Receive};

/// Object-safe trait for delivering a message to a subscriber process.
///
/// `#[async_trait]` is required so that `Box<dyn Dispatcher<T>>` is object-safe.
/// All implementors must be `'static + Send + Sync`.
#[async_trait]
pub(crate) trait Dispatcher<T>: 'static + Send + Sync {
    fn pid(&self) -> Pid;
    async fn dispatch(&self, msg: T) -> Result<(), SendError>;
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

    async fn dispatch(&self, msg: T) -> Result<(), SendError> {
        // Raw channel send bypasses `tell`'s internal dead-letter routing
        // (which would set sender: None). Stream's recv handles dead-letter
        // routing with the correct sender (Some(stream_pid)).
        let task: UserTask<P> = Box::new(TellTask::new(msg));
        self.user_tx.send(task).await.map_err(|_| SendError)
    }
}

pub(crate) struct PublishMsg<T>(pub(crate) T);
pub(crate) struct SubscribeMsg<T>(pub Box<dyn Dispatcher<T>>);
pub(crate) struct UnsubscribeMsg(pub Pid);

/// A built-in pub/sub process.
///
/// Topic-scoped: each unique topic name maps to exactly one `Stream` instance
/// in a `ProcessSystem`. Subscribers are stored as type-erased `Dispatcher<T>`,
/// keyed by PID for O(1) removal on termination or unsubscribe.
pub struct Stream<T = BoxedMessage> {
    subscribers: HashMap<Pid, Box<dyn Dispatcher<T>>>,
}

impl<T> Stream<T>
where
    T: 'static + Send + Sync,
{
    pub(crate) fn new() -> Self {
        Self {
            subscribers: HashMap::new(),
        }
    }
}

impl<T> Process for Stream<T>
where
    T: 'static + Send + Sync,
{
    /// Remove the terminated subscriber from the list.
    ///
    /// No unwatch needed: the target is already stopped.
    fn on_terminated(
        &mut self,
        terminated: Terminated,
        _ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = ()> + Send {
        self.subscribers.remove(&terminated.who);
        async {}
    }
}

/// Broadcast a published message to every subscriber.
///
/// Delivery failure for one subscriber does not abort delivery to the rest.
/// Failed dispatches are routed to the dead-letter stream with sender set to
/// this Stream's PID, distinguishing them from direct-tell failures (sender: None).
impl<T> Receive<PublishMsg<T>> for Stream<T>
where
    T: 'static + Send + Sync + Clone,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: PublishMsg<T>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        for dispatcher in self.subscribers.values() {
            if dispatcher.dispatch(msg.0.clone()).await.is_err() {
                if let Some(ref dl) = ctx.dead_letter {
                    let envelope = DeadLetterEnvelope {
                        destination: dispatcher.pid(),
                        message: BoxedMessage::new(msg.0.clone()),
                        sender: Some(ctx.pid()),
                        suppress_log: suppress_log::<T>(),
                        message_type_id: TypeId::of::<T>(),
                    };
                    dl.send(envelope).await;
                }
            }
        }
        Ok(())
    }
}

/// Register a new subscriber dispatcher and start watching it for termination.
impl<T> Receive<SubscribeMsg<T>> for Stream<T>
where
    T: 'static + Send + Sync,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: SubscribeMsg<T>,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        let pid = msg.0.pid();
        self.subscribers.insert(pid, msg.0);
        ctx.watch(pid).await;
        Ok(())
    }
}

/// Remove a subscriber by PID and stop watching it.
impl<T> Receive<UnsubscribeMsg> for Stream<T>
where
    T: 'static + Send + Sync,
{
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        msg: UnsubscribeMsg,
        ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        if self.subscribers.remove(&msg.0).is_some() {
            ctx.unwatch(msg.0).await;
        }
        Ok(())
    }
}

impl ProcessProxy<Stream<BoxedMessage>> {
    /// Publish any `Message` value to all subscribers of this stream.
    ///
    /// The value is type-erased into `BoxedMessage` so every subscriber receives
    /// the same zero-copy `Arc` clone. Use [`publish`][ProcessProxy::publish] on a
    /// typed `Stream<T>` instead when the stream message type is known.
    pub async fn publish_boxed<M: Message>(&self, msg: M) -> Result<(), SendError> {
        self.tell(PublishMsg(BoxedMessage::new(msg))).await
    }
}

impl<T> ProcessProxy<Stream<T>>
where
    T: 'static + Send + Sync + Clone,
{
    /// Publish a typed value to all subscribers of this stream.
    ///
    /// Symmetric counterpart of [`tell`][ProcessProxy::tell]: both accept a concrete
    /// message type, giving `publish` the same ergonomics as `tell`.
    pub async fn publish(&self, msg: T) -> Result<(), SendError> {
        self.tell(PublishMsg(msg)).await
    }
}

impl<T> ProcessProxy<Stream<T>>
where
    T: 'static + Send + Sync,
{
    /// Register `proxy` as a subscriber to this stream.
    ///
    /// The proxy's process must implement `Receive<T, Response = ()>`.
    pub async fn subscribe<P>(&self, proxy: ProcessProxy<P>) -> Result<(), SendError>
    where
        P: Process + Receive<T, Response = ()>,
    {
        self.tell(SubscribeMsg(Box::new(proxy))).await
    }

    /// Remove the subscriber identified by `pid` from this stream.
    ///
    /// No-op if `pid` is not currently subscribed.
    pub async fn unsubscribe(&self, pid: Pid) -> Result<(), SendError> {
        self.tell(UnsubscribeMsg(pid)).await
    }
}
