//! Defining, starting and talking to processes.
//!
//! A process is defined by a type that implements [`Process`], plus one
//! [`Receive<M>`](Receive) for each message type `M` it handles. Start it by
//! passing its [`Props`] to [`ProcessSystem::spawn`](crate::ProcessSystem::spawn),
//! then send it messages through the [`ProcessProxy`] that is returned. See
//! [Getting started](crate#getting-started) for a longer walkthrough.
//!
//! The following process keeps a running total. [`tell`](ProcessProxy::tell)
//! sends a message without waiting for a reply, and
//! [`ask`](ProcessProxy::ask) sends one and returns the handler's reply:
//!
//! ```
//! use std::convert::Infallible;
//!
//! use nitinol_runtime::process::{Process, ProcessContext, Receive};
//! use nitinol_runtime::{ProcessSystem, Props};
//!
//! struct Total(u32);
//!
//! impl Process for Total {}
//!
//! struct Add(u32);
//!
//! impl Receive<Add> for Total {
//!     type Response = u32;
//!     type Error = Infallible;
//!
//!     async fn recv(
//!         &mut self,
//!         msg: Add,
//!         _ctx: &mut ProcessContext<Self>,
//!     ) -> Result<u32, Infallible> {
//!         self.0 += msg.0;
//!         Ok(self.0)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let system = ProcessSystem::new().await;
//!     let total = system.spawn(Props::new(|| Total(0))).await;
//!
//!     total.tell(Add(2)).await.expect("tell failed");
//!     let sum = total.ask(Add(3)).await.expect("ask failed");
//!     assert_eq!(sum, 5);
//!
//!     total.stop().await.expect("stop failed");
//! }
//! ```
//!
//! The main types in this module:
//!
//! - [`Process`]: the state of a process and its lifecycle hooks.
//! - [`Receive`]: the handler for one message type.
//! - [`ProcessContext`]: what a handler or hook can do with its own process.
//! - [`ProcessProxy`]: a typed handle to a running process.
//! - [`AnyProxy`]: a handle whose process type is not known, as returned by
//!   lookups.
//! - [`Props`]: how to start a process.
//! - [`SupervisionStrategy`]: what happens when a handler returns an error.
//! - [`Terminated`]: the notice a watcher receives when a process ends.
//! - [`Stream`], [`StreamProps`] and [`Subscriber`]: publish/subscribe.
//! - [`DeadLetter`]: a message that could not be delivered.

mod context;
mod dead_letter;
mod driver;
mod lifecycle;
mod message;
mod pid_set;
mod props;
mod proxy;
mod registry;
mod resident;
mod signal;
pub(crate) mod spawn;
pub(crate) mod spawnable;
mod stream;
mod subscriber;
pub(crate) mod supervision;
pub(crate) mod task;
pub(crate) mod watch;
mod wiring;

pub use self::{
    context::ProcessContext,
    dead_letter::{DeadLetter, SuppressDeadLetterLog},
    driver::{Combine, DefaultDriver, Driver, PipeDriver, PipeHandle, PipePanic},
    message::*,
    pid_set::PidSet,
    props::*,
    proxy::*,
    resident::Resident,
    stream::{Stream, StreamProps},
    subscriber::{Subscriber, SubscriberContext},
    watch::{Terminated, TerminatedReason},
};

pub(crate) use self::dead_letter::{DeadLetterProcess, DeadLetterProxy};
pub(crate) use self::registry::*;
pub(crate) use self::resident::ResidentTable;

use std::future::Future;

/// The state of a process.
///
/// Each spawned process runs on its own Tokio task and handles its messages
/// one at a time. The lifecycle hooks `on_start`, `on_stop` and
/// `on_terminated` do nothing unless overridden.
#[allow(unused_variables)]
pub trait Process: 'static + Sync + Send {
    /// Called when the process starts and again after each restart, before it
    /// handles any message. Returns nothing.
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        async {}
    }
    /// Called when the process stops, and before each restart. Not called when
    /// the process is poisoned. Returns nothing.
    fn on_stop(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        async {}
    }
    /// Called with a [`Terminated`] notice when a process this process watches
    /// ends. Returns nothing.
    fn on_terminated(
        &mut self,
        terminated: Terminated,
        ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        async {}
    }
}

/// The handler for messages of type `M`.
///
/// `Response` is the reply that [`ProcessProxy::ask`] returns. Returning
/// `Err` hands the error to the process's [`SupervisionStrategy`].
pub trait Receive<M>: Process {
    /// The reply to a message of type `M`.
    type Response: 'static + Send;
    /// The error a handler for `M` returns.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Called for each message of type `M` the process receives. Returns the
    /// reply, or an error that is handed to the [`SupervisionStrategy`].
    fn recv(
        &mut self,
        msg: M,
        ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> + Send
    where
        Self: Sized;
}
