mod context;
mod dead_letter;
mod lifecycle;
mod message;
mod props;
mod proxy;
mod registry;
mod signal;
mod stream;
mod subscriber;
pub(crate) mod supervision;
pub(crate) mod task;
pub(crate) mod watch;

pub use self::{
    context::*,
    dead_letter::{DeadLetter, SuppressDeadLetterLog},
    message::*,
    props::*,
    proxy::*,
    stream::Stream,
    subscriber::Subscriber,
    watch::{Terminated, TerminatedReason},
};

pub(crate) use self::dead_letter::{DeadLetterProcess, DeadLetterProxy};
pub(crate) use self::lifecycle::run;
pub(crate) use self::registry::*;

use std::future::Future;

#[allow(unused_variables)]
pub trait Process: 'static + Sync + Send {
    fn on_start(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn on_stop(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn on_terminated(
        &mut self,
        terminated: Terminated,
        ctx: &mut ProcessContext,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }
}

pub trait Receive<M>: Process {
    type Response: 'static + Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn recv(
        &mut self,
        msg: M,
        ctx: &mut ProcessContext,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> + Send;
}
