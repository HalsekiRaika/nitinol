mod event;
mod command;
mod entrust;
mod receive;

pub use self::event::*;
pub use self::command::*;
pub use self::entrust::*;
pub use self::receive::*;

use async_trait::async_trait;

use crate::{Process, Context};
use crate::errors::ChannelDropped;

#[async_trait]
pub trait TaskApplier<T: Process>: 'static + Sync + Send {
    async fn apply(self: Box<Self>, state: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped>;
}
