use futures_core::future::BoxFuture;

use crate::aggregate::Aggregate;
use crate::decider::Decider;
use crate::error::TellError;
use crate::process::proxy::AggregateProxy;

/// Implementors must be `Clone + Send + Sync + 'static` so a saga producer
/// closure can capture the target and clone it for each (re-)spawn.
pub trait AggregateTellTarget<A: Aggregate>: Clone + Send + Sync + 'static {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static;
}

impl<A: Aggregate> AggregateTellTarget<A> for AggregateProxy<A> {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
    {
        Box::pin(AggregateProxy::tell(self, cmd))
    }
}
