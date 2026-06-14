mod context;
mod dead_letter;
mod driver;
mod lifecycle;
mod message;
mod pid_set;
mod props;
mod proxy;
mod registry;
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
    driver::{Combine, Driver, PipeDriver, PipeHandle, PipePanic},
    message::*,
    pid_set::PidSet,
    props::*,
    proxy::*,
    stream::{Stream, StreamProps},
    subscriber::{Subscriber, SubscriberContext},
    watch::{Terminated, TerminatedReason},
};

pub(crate) use self::dead_letter::{DeadLetterProcess, DeadLetterProxy};
pub(crate) use self::registry::*;

use std::future::Future;

#[allow(unused_variables)]
pub trait Process: 'static + Sync + Send {
    fn on_start(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        async {}
    }
    fn on_stop(&mut self, ctx: &mut ProcessContext<Self>) -> impl Future<Output = ()> + Send
    where
        Self: Sized,
    {
        async {}
    }
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

pub trait Receive<M>: Process {
    type Response: 'static + Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn recv(
        &mut self,
        msg: M,
        ctx: &mut ProcessContext<Self>,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> + Send
    where
        Self: Sized;
}
