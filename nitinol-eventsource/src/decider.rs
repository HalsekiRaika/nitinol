use async_trait::async_trait;

use crate::aggregate::Aggregate;
use crate::context::Context;
use crate::Effect;

/// Decides which [`Effect`]s to produce for a given command.
///
/// `decide` receives the current aggregate state as `&self` (shared reference),
/// preventing any state mutation during the decision.  State changes happen
/// exclusively through [`Aggregate::apply`] after the events are
/// committed.
///
/// # `Rejection` semantics
///
/// `Rejection` represents **domain-rule violations only** — for example,
/// "cannot withdraw more than the current balance".  Infrastructure errors
/// (I/O failures, serialization errors) must not be modeled as `Rejection`;
/// they propagate through a separate `Result` path at the infrastructure layer.
///
/// # Why there is no Reply type
///
/// `decide` returns `Effect<E>`, not a reply value.  The caller receives the
/// persisted events from [`crate::AggregateProxy::ask`] and derives any
/// needed response directly from those events.  This limits the Decider's
/// responsibility to "declare what happened", without coupling it to the
/// caller's view of the world.
///
/// # Example
///
/// ```rust,no_run
/// use async_trait::async_trait;
/// use nitinol_eventsource::{Aggregate, Decider, Effect, Event, Context};
/// use nitinol_persistence::{EventType, Family, TypeName};
///
/// #[derive(Clone)]
/// enum CounterEvent { Incremented }
/// impl Event for CounterEvent { const EVENT_TYPE: EventType = EventType::new(Family::new("counter"), TypeName::new("incremented")); }
///
/// struct Increment;
///
/// #[derive(Default, thiserror::Error, Debug)]
/// #[error("overflow")]
/// struct CounterError;
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
/// impl Decider<Increment> for Counter {
///     type Rejection = CounterError;
///
///     async fn decide(&self, _: Increment, _: &mut Context)
///         -> Result<Effect<CounterEvent>, CounterError>
///     {
///         Ok(Effect::persist(CounterEvent::Incremented))
///     }
/// }
/// ```
#[async_trait]
pub trait Decider<C>: Aggregate
where
    C: Send + Sync + 'static,
{
    type Rejection: std::error::Error + Send + Sync + 'static;

    async fn decide(
        &self,
        cmd: C,
        ctx: &mut Context,
    ) -> Result<Effect<Self::Event>, Self::Rejection>;
}
