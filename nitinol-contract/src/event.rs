use nitinol_persistence::EventType;

use crate::codec::Codec;

/// Marker trait for domain events.
///
/// `Clone` is required because `ask` returns the persisted events as a `Vec<E>`
/// while simultaneously moving them into `Aggregate::apply`.
pub trait Event: Clone + Send + Sync + 'static {
    /// The one identity every row of this event carries.
    ///
    /// An enum event is written down under this one path too: which arm a value
    /// holds is carried by the payload [`Self::Codec`] writes, so the column
    /// names the declaration and the payload names the case.
    const EVENT_TYPE: EventType;

    /// The codec that writes this event down and reads it back.
    ///
    /// The format belongs to the event rather than to the wiring that later
    /// activates it: an event and its bytes are one decision, and a stream
    /// written by one format cannot be read by another. Naming it here is what
    /// lets an interpreter resolve the codec from the event type alone, with
    /// nothing to inject and nothing to keep in step.
    type Codec: Codec<Self>;
}

/// A type that states the one [`EventType`] every row of it carries.
///
/// [`Event`] states it for a domain event and `SystemEvent` states it for a
/// record the framework keeps of its own; both reach the same `event_type`
/// column, so the question "is this stored identity the one this type declares?"
/// is one question and is answered here once, through
/// [`MatchesEventType::matches`].
///
/// # Why the two families do not collide
///
/// The blanket implementation below covers every [`Event`], and a framework
/// record is covered by an implementation written out for it by hand. The two
/// never overlap because `Event` and `SystemEvent` are independent — neither is
/// a super-trait of the other and no type implements both — so a framework
/// record is not an `Event` and the blanket does not reach it. That
/// independence is what makes the hand-written implementations legal; merging
/// the two traits would make them conflict.
///
/// Not a user surface: a domain type is covered by declaring [`Event`], and
/// nothing is gained by naming this trait.
#[doc(hidden)]
pub trait DeclaresEventType {
    /// The identity this type's rows are written down under.
    const EVENT_TYPE: EventType;
}

impl<E: Event> DeclaresEventType for E {
    const EVENT_TYPE: EventType = <E as Event>::EVENT_TYPE;
}

/// Asking a stored identity whether it is the one a given type declared.
///
/// Stated as an extension of [`EventType`] rather than as an inherent method
/// because the declaration lives here, in the contract, while `EventType` is
/// the persistence layer's and knows nothing of it.
///
/// A consumer imports this trait and not [`DeclaresEventType`]: the latter
/// carries an `EVENT_TYPE` of its own, and bringing that name into a module
/// that already spells `SomeEvent::EVENT_TYPE` would make those references
/// ambiguous.
#[doc(hidden)]
pub trait MatchesEventType {
    /// Is this the identity `T` declares?
    ///
    /// An identity is a family and a type name together, so both have to agree:
    /// a sibling declaration in the same family answers for its own path alone.
    fn matches<T: DeclaresEventType>(self) -> bool;
}

impl MatchesEventType for EventType {
    fn matches<T: DeclaresEventType>(self) -> bool {
        self == <T as DeclaresEventType>::EVENT_TYPE
    }
}
