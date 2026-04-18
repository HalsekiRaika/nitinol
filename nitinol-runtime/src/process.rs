pub(crate) mod task;
mod proxy;
mod context;
mod signal;
mod lifecycle;
mod registry;
mod props;

pub use self::{
    proxy::*,
    context::*,
    props::*,
};

pub(crate) use self::registry::*;
pub(crate) use self::lifecycle::run;

use std::future::Future;

use crate::error::BoxError;

pub trait Process: 'static + Sync + Send {
    fn on_start(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn on_stop(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

pub trait Receive<M>: Process {
    type Response: 'static + Send;
    fn receive(
        &mut self,
        msg: M,
    ) -> impl Future<Output = Result<Self::Response, BoxError>> + Send;
}
