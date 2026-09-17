use nitinol_persistence::EventType;

use crate::codec::Codec;

/// Marker trait for domain events.
///
/// `Clone` is required because `ask` returns the persisted events as a `Vec<E>`
/// while simultaneously moving them into `Aggregate::apply`.
pub trait Event: Clone + Send + Sync + 'static {
    const EVENT_TYPE: EventType;

    /// The codec that writes this event down and reads it back.
    ///
    /// The format belongs to the event rather than to the wiring that later
    /// activates it: an event and its bytes are one decision, and a stream
    /// written by one format cannot be read by another. Naming it here is what
    /// lets an interpreter resolve the codec from the event type alone, with
    /// nothing to inject and nothing to keep in step.
    type Codec: Codec<Self>;

    /// Value-level identity of this event.
    ///
    /// Defaults to the type-level [`Self::EVENT_TYPE`] (variant `None`). Enum
    /// events override this to fill in the active arm's variant.
    fn variant(&self) -> EventType {
        Self::EVENT_TYPE
    }
}
