use futures_core::future::BoxFuture;
use nitinol_runtime::process::{ProcessProxy, Stream};
use nitinol_runtime::{BoxedMessage, Message};

use crate::effect::core::{SideEffect, SideEffectError};

/// A side effect that publishes a typed message to a `Stream<BoxedMessage>`.
///
/// The stream proxy is fixed to `ProcessProxy<Stream<BoxedMessage>>` because
/// `PublishMsg<T>` is crate-private in `nitinol-runtime`.  The message is
/// type-erased into `BoxedMessage` on execution.
pub(crate) struct TypedPublish<M: Message> {
    pub(crate) stream: ProcessProxy<Stream<BoxedMessage>>,
    pub(crate) message: M,
}

impl<M: Message> SideEffect for TypedPublish<M> {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.stream
                .publish(self.message)
                .await
                .map_err(|e| SideEffectError::Send(Box::new(e)))
        })
    }
}
