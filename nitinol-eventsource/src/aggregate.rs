use crate::event::Event;

/// Core trait for event-sourced aggregates.
///
/// An aggregate holds domain state and evolves it by applying domain events.
/// It is the central building block of the CQRS + Event Sourcing pattern:
/// commands are decided by [`crate::Decider`], and the resulting events are
/// applied here to advance the state.
///
/// # Why `apply` is synchronous
///
/// `apply` is a pure state-transition function — it requires no I/O.
/// Making it `async` would force every implementor to interact with an async
/// executor and would significantly complicate derived traits (e.g.
/// [`Snapshotable`]).  Keeping it synchronous also makes event replay
/// deterministic and straightforward to test without a runtime.
///
/// State mutation only happens inside `apply`; `Decider::decide` receives
/// `&self` and must not mutate state.
///
/// # Example
///
/// ```rust,no_run
/// use nitinol_eventsource::{Aggregate, Event};
/// use nitinol_persistence::EventType;
///
/// #[derive(Clone)]
/// enum CounterEvent { Incremented }
/// impl Event for CounterEvent { const EVENT_TYPE: EventType = EventType::from_str("counter.incremented"); }
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
/// ```
pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: Event;

    fn apply(&mut self, event: Self::Event);
}

/// Enables an aggregate to be checkpointed as a serialisable snapshot value.
///
/// `type Snapshot` is the pure domain value captured and restored; byte
/// serialisation is handled externally by a [`crate::codec::Codec<Self::Snapshot>`].
pub trait Snapshotable: Sized {
    /// The pure domain value that represents a point-in-time snapshot of this
    /// aggregate's state.
    type Snapshot;

    /// Capture the current state as a snapshot value.
    fn capture(&self) -> Self::Snapshot;

    /// Restore aggregate state from a snapshot value.
    fn restore(snapshot: Self::Snapshot) -> Self;
}
