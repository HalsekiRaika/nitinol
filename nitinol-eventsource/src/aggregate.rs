use crate::event::Event;

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
