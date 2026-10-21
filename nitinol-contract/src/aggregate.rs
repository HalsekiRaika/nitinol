use crate::codec::Codec;
use crate::event::Event;
use crate::recover::Recovery;

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
/// use nitinol_contract::{Aggregate, Codec, Event, Never};
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
///     type Recover = Never;
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

    /// How this aggregate's state is reproduced before it decides anything:
    /// [`Never`](crate::Never) to replay the whole stream, or
    /// [`Latest`](crate::Latest) to recover from the newest snapshot and replay
    /// only what was written after it.
    ///
    /// Declared by the aggregate rather than at the wiring, because whether a
    /// snapshot exists to recover from is a property of the type — it is the
    /// same type that names the snapshot value and the codec which writes it.
    /// Stating it twice would leave two places to disagree.
    ///
    /// There is no default. An aggregate says which of the two it is, and
    /// [`Latest`](crate::Latest) is available only where [`Snapshotable`] is
    /// implemented.
    type Recover: Recovery<Self>;

    fn apply(&mut self, event: Self::Event);
}

/// Enables an aggregate to be checkpointed as a serializable snapshot value.
///
/// `type Snapshot` is the pure domain value captured and recovered, and
/// `type Codec` is the format it is written in — named here, next to the value
/// it writes, for the same reason [`Event::Codec`] is named on the event.
///
/// # Replay acceleration
///
/// Implementing this says a snapshot *can* be taken and read back. Whether one
/// is reached for at start is [`Aggregate::Recover`]: an aggregate that names
/// [`Latest`](crate::Latest) recovers from the newest snapshot and replays only
/// the events written after it, and one that names [`Never`](crate::Never)
/// replays its stream from the beginning. When no snapshot has been saved yet,
/// a `Latest` aggregate replays from the start too.
///
/// When to capture a snapshot is an application-level decision: this trait
/// only describes how to materialize and recover one. The caller issues saves
///  (typically through the snapshot persistor) at whatever sequence
/// boundary the application chooses.
pub trait Snapshotable: Sized {
    /// The pure domain value that represents a point-in-time snapshot of this
    /// aggregate's state.
    type Snapshot;

    /// The codec that writes this aggregate's snapshot down and reads it back.
    ///
    /// Independent of [`Event::Codec`]: a snapshot is a different value in a
    /// different format, and an activation that recovers from one reaches for
    /// this type rather than for the one that reads the stream.
    type Codec: Codec<Self::Snapshot>;

    /// Capture the current state as a snapshot value.
    fn capture(&self) -> Self::Snapshot;

    /// Recover aggregate state from a snapshot value.
    ///
    /// The name is the vocabulary: an event is *replayed*, a snapshot is
    /// *recovered*. There is no `restore` beside it: a correct implementor
    /// defines this method, and a reference to `<T as Snapshotable>::restore`
    /// does not resolve:
    ///
    /// ```compile_fail
    /// # use bytes::Bytes;
    /// # use nitinol_contract::{Codec, Snapshotable};
    /// # struct CountCodec;
    /// # impl Codec<u64> for CountCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &u64) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<u64, Self::Error> { Ok(0) }
    /// # }
    /// struct Counter {
    ///     value: u64,
    /// }
    ///
    /// impl Snapshotable for Counter {
    ///     type Snapshot = u64;
    ///     type Codec = CountCodec;
    ///
    ///     fn capture(&self) -> u64 {
    ///         self.value
    ///     }
    ///
    ///     fn recover(snapshot: u64) -> Self {
    ///         Self { value: snapshot }
    ///     }
    /// }
    ///
    /// let _ = <Counter as Snapshotable>::restore(0);
    /// ```
    fn recover(snapshot: Self::Snapshot) -> Self;
}
