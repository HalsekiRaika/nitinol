mod applicator;
mod publisher;
mod employ;

pub use self::applicator::*;
pub use self::publisher::*;
pub use self::employ::*;

use async_trait::async_trait;
use crate::{Process, Context};
use crate::errors::ChannelDropped;

#[async_trait]
pub trait ProcessApplier<T: Process>: 'static + Sync + Send {
    async fn apply(self: Box<Self>, entity: &mut T, ctx: &mut Context) -> Result<(), ChannelDropped>;
}
