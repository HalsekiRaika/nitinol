use nitinol_persistence::EventType;

pub trait Event: Send + Sync + 'static {
    const EVENT_TYPE: EventType;
}
