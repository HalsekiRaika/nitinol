use async_trait::async_trait;

use crate::aggregate::Aggregate;
use crate::context::Context;

/// Handles read-only queries against the current aggregate state.
///
/// `recv` is invoked on the **exec** path only ([`crate::AggregateProxy::exec`]).
/// It must not produce side effects or mutate state; it reads `&self` and
/// returns a `Response`.
///
/// Unlike [`crate::Decider::decide`], `recv` never persists events.  Use it
/// when the caller needs to inspect the current state without triggering a
/// command-decision cycle.
///
/// # Example
///
/// ```rust,no_run
/// use async_trait::async_trait;
/// use nitinol_eventsource::{Aggregate, Receive, Event, Context};
/// use nitinol_persistence::{EventType, Family, TypeName};
///
/// #[derive(Clone)]
/// enum CounterEvent { Incremented }
/// impl Event for CounterEvent { const EVENT_TYPE: EventType = EventType::new(Family::new("counter"), TypeName::new("incremented")); }
///
/// struct GetCount;
///
/// #[derive(Default)]
/// struct Counter { value: u64 }
///
/// impl Aggregate for Counter {
///     type Event = CounterEvent;
///     fn apply(&mut self, event: CounterEvent) { /* … */ }
/// }
///
/// #[async_trait]
/// impl Receive<GetCount> for Counter {
///     type Response = u64;
///     type Error = std::convert::Infallible;
///
///     async fn recv(&self, _: GetCount, _: &mut Context)
///         -> Result<u64, std::convert::Infallible>
///     {
///         Ok(self.value)
///     }
/// }
/// ```
#[async_trait]
pub trait Receive<M>: Aggregate
where
    M: Send + Sync + 'static,
{
    type Response: Send + Sync + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    async fn recv(&self, msg: M, ctx: &mut Context) -> Result<Self::Response, Self::Error>;
}
