mod publisher;
mod subscriber;

pub use self::publisher::*;
pub use self::subscriber::*;

use nitinol_process::{Context, FromContextExt};
use nitinol_process::extension::errors::Missing;

use crate::eventstream::EventStream;

#[derive(Clone)]
pub struct EventStreamExtension(pub(crate) EventStream);

impl EventStreamExtension {
    pub fn new(stream: EventStream) -> Self {
        Self(stream)
    }
}

impl AsRef<EventStream> for EventStreamExtension {
    fn as_ref(&self) -> &EventStream {
        &self.0
    }
}

impl FromContextExt for EventStreamExtension {
    fn from_context(ctx: &Context) -> Result<&Self, Missing> {
        ctx.extension().get()
    }
}