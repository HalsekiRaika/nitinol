mod event;
mod command;
mod forget;

pub use self::event::*;
pub use self::command::*;
pub use self::forget::*;

use async_trait::async_trait;

use crate::{Process, Context};
use crate::errors::ChannelDropped;

#[async_trait]
pub trait ProcessApplier<T: Process>: 'static + Sync + Send {
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped>;
}
