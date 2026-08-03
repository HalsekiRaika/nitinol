use futures_core::future::BoxFuture;

use crate::aggregate::Aggregate;
use crate::decider::Decider;
use crate::error::TellError;
use crate::process::proxy::AggregateProxy;

/// Implementors must be `Clone + Send + Sync + 'static` so a producer
/// closure can capture the target and clone it for each dispatch.
pub trait AggregateTellTarget<A: Aggregate>: Clone + Send + Sync + 'static {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static;

    /// The aggregate's stream key.
    ///
    /// Used by higher-level consumers (e.g. [`TellIntent`][crate::process]) to
    /// identify this target without a round-trip to the aggregate's process.
    ///
    /// Implementations **must** return the actual stream key of the target
    /// aggregate.  Returning an empty string is rejected at
    /// [`TellIntent::new`][crate::process] construction time with a panic.
    ///
    /// [`AggregateProxy`] provides this automatically from the aggregate id.
    fn aggregate_id_str(&self) -> &str;
}

impl<A: Aggregate> AggregateTellTarget<A> for AggregateProxy<A> {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(AggregateProxy::tell(self, cmd))
    }

    fn aggregate_id_str(&self) -> &str {
        self.aggregate_id().as_str()
    }
}
