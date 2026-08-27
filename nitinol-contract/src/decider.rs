use crate::aggregate::Aggregate;
use crate::decision::Decision;

/// Decides what a command means for the current state.
///
/// `decide` is pure, synchronous and deterministic: the same state and the same
/// command always yield the same decision (L-1). It reads `&self` and returns a
/// value, so it can neither move state — that happens only in
/// [`Aggregate::apply`], after an interpreter has persisted the events — nor
/// perform I/O, which the absence of an `async` signature states in the type
/// rather than in prose. There is no extension point for an asynchronous
/// decision: a decider that needs to consult the world is asking for a fact it
/// should have been given.
///
/// # One command type, one answer type
///
/// `Output` and `Rejection` are fixed per command type rather than per
/// aggregate, because the answer to "how much is left?" and the answer to "who
/// now owns this?" have no reason to be the same type. A command that asks
/// nothing declares `type Output = ()`. That is deliberately not an `Option`:
/// the decider says once, on the impl, that the command has no answer, instead
/// of every decision restating it.
///
/// `Rejection` describes domain-rule violations only (L-6). Failures of the
/// machinery around the decision — the store, the mailbox, concurrency control
/// — are not decisions and are reported by the interpreter's own error type.
///
/// # Example
///
/// ```rust
/// use nitinol_contract::{Aggregate, Decider, Decision, Event};
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
/// struct Increment;
/// struct AtCeiling;
///
/// impl Decider<Increment> for Counter {
///     type Output = u64;
///     type Rejection = AtCeiling;
///
///     fn decide(&self, _: Increment) -> Decision<CounterEvent, u64, AtCeiling> {
///         let Some(next) = self.value.checked_add(1) else {
///             return Decision::reject(AtCeiling);
///         };
///         Decision::persist(vec![CounterEvent::Incremented]).output(next)
///     }
/// }
///
/// let decision = Counter::default().decide(Increment);
///
/// assert!(matches!(decision, Decision::Accept { output: 1, .. }));
/// ```
pub trait Decider<C>: Aggregate {
    /// The answer this command asks for; `()` when it asks for none.
    type Output;

    /// The domain-rule violations that can refuse this command.
    type Rejection;

    fn decide(&self, cmd: C) -> Decision<Self::Event, Self::Output, Self::Rejection>;
}
