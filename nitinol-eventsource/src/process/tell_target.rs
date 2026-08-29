use futures_core::future::BoxFuture;
use nitinol_contract::{Aggregate, Decider};
use nitinol_persistence::AggregateId;

use crate::error::TellError;
use crate::process::proxy::AggregateProxy;

/// Implementors must be `Clone + Send + Sync + 'static` so a producer
/// closure can capture the target and clone it for each dispatch.
///
/// `Rejection` is bounded here rather than on the contract's [`Decider`]: a
/// refusal that reaches a told command has no caller to be returned to, so the
/// interpreter reports it instead, and it can only do that for a rejection it
/// can render.  The contract itself stays free of that requirement, because a
/// decider tested in isolation owes nobody a report.
pub trait AggregateTellTarget<A: Aggregate>: Clone + Send + Sync + 'static {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static;

    /// The id of the aggregate this target dispatches to — and, verbatim, the
    /// key of the stream that aggregate persists to.
    ///
    /// Used by higher-level consumers (e.g. a saga's tell intent) to identify
    /// this target without a round-trip to the aggregate's process.
    ///
    /// Implementations **must** return the id of the actual target aggregate.
    /// An empty [`AggregateId`] is a legitimate value in the framework at large
    /// — it is how "no aggregate" is spelled where that is a meaningful state —
    /// so the type cannot rule emptiness out on this accessor's behalf.  A
    /// consumer for which an empty target would be meaningless rejects it at
    /// its own construction boundary instead.
    ///
    /// [`AggregateProxy`] provides this automatically from the aggregate id.
    fn aggregate_id(&self) -> &AggregateId;
}

impl<A: Aggregate> AggregateTellTarget<A> for AggregateProxy<A> {
    fn tell<C>(&'_ self, cmd: C) -> BoxFuture<'_, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        Box::pin(AggregateProxy::tell(self, cmd))
    }

    fn aggregate_id(&self) -> &AggregateId {
        AggregateProxy::aggregate_id(self)
    }
}
