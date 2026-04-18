mod context;
mod lifecycle;
mod props;
mod proxy;
mod registry;
mod signal;
pub(crate) mod task;

pub use self::{context::*, props::*, proxy::*};

pub(crate) use self::lifecycle::run;
pub(crate) use self::registry::*;

use std::future::Future;

use crate::error::BoxError;

#[allow(unused_variables)]
pub trait Process: 'static + Sync + Send {
    fn on_start(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn on_stop(&mut self, ctx: &mut ProcessContext) -> impl Future<Output = ()> + Send {
        async {}
    }
}

pub trait Receive<M>: Process {
    type Response: 'static + Send;
    fn recv(&mut self, msg: M, ctx: &mut ProcessContext) -> impl Future<Output = Result<Self::Response, BoxError>> + Send;
}
