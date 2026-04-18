pub mod task;
mod proxy;
mod context;
mod signal;
mod lifecycle;
mod registry;

pub use self::{
    proxy::*,
    context::*,
};

pub(crate) use self::registry::*;

use std::future::Future;

pub trait Process: 'static + Sync + Send {
    fn on_start(&mut self) -> impl Future<Output=()> + Send { async {} }
    fn on_stop(&mut self) -> impl Future<Output=()> + Send { async {} }
}

pub trait Receive<M>: Process {
    fn receive(&mut self, msg: M) -> impl Future<Output=Result<(), ()>> + Send;
}
