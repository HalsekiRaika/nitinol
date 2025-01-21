use nitinol_process::{Context, FromContextExt};
use nitinol_process::extension::errors::Missing;
use crate::eventstream::EventStream;

mod publisher;

pub use self::publisher::*;

#[derive(Clone, Default)]
pub struct EventStreamExtension(pub(crate) EventStream);

impl FromContextExt for EventStreamExtension {
    fn from_context(ctx: &Context) -> Result<&Self, Missing> {
        ctx.extension().get()
    }
}