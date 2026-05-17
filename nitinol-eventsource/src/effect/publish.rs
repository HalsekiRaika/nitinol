use futures_core::future::BoxFuture;
use nitinol_runtime::process::{ProcessProxy, Stream};
use nitinol_runtime::Message;

use crate::effect::core::{SideEffect, SideEffectError};

pub(crate) struct TypedPublish<T: Message + Clone> {
    pub(crate) stream: ProcessProxy<Stream<T>>,
    pub(crate) message: T,
}

impl<T: Message + Clone> SideEffect for TypedPublish<T> {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.stream
                .publish(self.message)
                .await
                .map_err(|e| SideEffectError::Send(Box::new(e)))
        })
    }
}
