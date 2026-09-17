use crate::codec::Codec;
use crate::event::Event;

/// Core trait for event-sourced aggregates.
///
/// An aggregate holds domain state and evolves it by applying domain events.
/// It is the central building block of the CQRS and Event Sourcing pattern:
/// commands are decided by the pure, synchronous [`Decider`](crate::Decider)
/// contract defined in this crate, and the events of the resulting
/// [`Decision`](crate::Decision) are applied here to advance the state.
///
/// # Why `apply` is synchronous
///
/// `apply` is a pure state-transition function — it requires no I/O.
/// Making it `async` would force every implementor to interact with an async
/// executor and would significantly complicate derived traits (e.g.
/// [`Snapshotable`]).  Keeping it synchronous also makes event replay
/// deterministic and straightforward to test without a runtime.
///
/// State mutation only happens inside `apply`; a decider receives `&self` and
/// must not mutate state.
///
/// # Example
///
/// ```rust,no_run
/// use bytes::Bytes;
/// use nitinol_contract::{Aggregate, Codec, Event};
/// use nitinol_persistence::{EventType, Family, TypeName};
///
/// #[derive(Clone)]
/// enum CounterEvent { Incremented }
///
/// struct CounterEventCodec;
/// impl Codec<CounterEvent> for CounterEventCodec {
///     type Error = std::convert::Infallible;
///     fn encode(_: &CounterEvent) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
///     fn decode(_: &[u8]) -> Result<CounterEvent, Self::Error> { Ok(CounterEvent::Incremented) }
/// }
///
/// impl Event for CounterEvent {
///     const EVENT_TYPE: EventType = EventType::new(Family::new("counter"), TypeName::new("incremented"));
///     type Codec = CounterEventCodec;
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
/// ```
pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: Event;

    fn apply(&mut self, event: Self::Event);
}

/// Enables an aggregate to be checkpointed as a serializable snapshot value.
///
/// `type Snapshot` is the pure domain value captured and restored, and
/// `type Codec` is the format it is written in — named here, next to the value
/// it writes, for the same reason [`Event::Codec`] is named on the event.
///
/// # Replay acceleration
///
/// When a snapshot exists for an aggregate, replay starts from the snapshot
/// and only events written after it are reapplied — full replay from the
/// beginning of the stream is skipped. When no snapshot exists, every event
/// is replayed from the start.
///
/// When to capture a snapshot is an application-level decision: this trait
/// only describes how to materialize and restore one. The caller issues saves
///  (typically through the snapshot persistor) at whatever sequence
/// boundary the application chooses.
pub trait Snapshotable: Sized {
    /// The pure domain value that represents a point-in-time snapshot of this
    /// aggregate's state.
    type Snapshot;

    /// The codec that writes this aggregate's snapshot down and reads it back.
    ///
    /// Independent of [`Event::Codec`]: a snapshot is a different value in a
    /// different format, and an activation that restores one reaches for this
    /// type rather than for the one that reads the stream.
    type Codec: Codec<Self::Snapshot>;

    /// Capture the current state as a snapshot value.
    fn capture(&self) -> Self::Snapshot;

    /// Restore aggregate state from a snapshot value.
    fn restore(snapshot: Self::Snapshot) -> Self;
}
