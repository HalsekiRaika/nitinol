use nitinol_persistence::EventType;

/// Marker trait for domain events.
///
/// `Clone` is required because `ask` returns the persisted events as a `Vec<E>`
/// while simultaneously moving them into `Aggregate::apply`.
pub trait Event: Clone + Send + Sync + 'static {
    const EVENT_TYPE: EventType;
}
