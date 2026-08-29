use crate::aggregate::Aggregate;

/// Asks the current state a question.
///
/// `query` is pure, synchronous and deterministic: the same state and the same
/// message always yield the same answer (L-1). It reads `&self` and produces no
/// events, so asking a question can never move the state or reach the store —
/// which is what separates it from [`Decider`](crate::Decider): a decision
/// concludes what happened, a query only reports what is.
///
/// `Error` is the domain's own answer to a question it cannot answer — "this
/// wallet has no label" — not a failure of the machinery that carried the
/// question.
///
/// # Example
///
/// ```rust
/// use nitinol_contract::{Aggregate, Event, Query};
/// use nitinol_persistence::{EventType, Family, TypeName};
///
/// #[derive(Clone)]
/// enum CounterEvent { Incremented }
/// impl Event for CounterEvent {
///     const EVENT_TYPE: EventType =
///         EventType::new(Family::new("counter"), TypeName::new("incremented"));
/// }
///
/// #[derive(Default)]
/// struct Counter { value: u64 }
///
/// impl Aggregate for Counter {
///     type Event = CounterEvent;
///
///     fn apply(&mut self, event: CounterEvent) {
///         match event {
///             CounterEvent::Incremented => self.value += 1,
///         }
///     }
/// }
///
/// struct CurrentValue;
///
/// impl Query<CurrentValue> for Counter {
///     type Response = u64;
///     type Error = std::convert::Infallible;
///
///     fn query(&self, _: CurrentValue) -> Result<u64, std::convert::Infallible> {
///         Ok(self.value)
///     }
/// }
///
/// assert_eq!(Counter::default().query(CurrentValue), Ok(0));
/// ```
pub trait Query<M>: Aggregate {
    /// The answer to this question.
    type Response;

    /// Why the domain cannot answer it.
    type Error;

    fn query(&self, msg: M) -> Result<Self::Response, Self::Error>;
}
