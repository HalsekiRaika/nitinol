//! Custom `SideEffect` implementations for the aggregate-communication example.

use std::sync::Arc;

use futures_core::future::BoxFuture;
use tokio::sync::Notify;

use nitinol_eventsource::{AggregateProxy, SideEffect, SideEffectError};

use crate::counter::{Counter, Increment};

/// Side effect: sends `Increment` to a target `Counter` aggregate, then notifies `done`.
pub struct TellTargetEffect {
    pub target: AggregateProxy<Counter>,
    pub done: Arc<Notify>,
}

impl SideEffect for TellTargetEffect {
    fn execute(self: Box<Self>) -> BoxFuture<'static, Result<(), SideEffectError>> {
        Box::pin(async move {
            self.target
                .tell(Increment)
                .await
                .map_err(|e| SideEffectError::Send(Box::new(e)))?;
            self.done.notify_one();
            Ok(())
        })
    }
}
