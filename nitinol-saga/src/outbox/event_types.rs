use nitinol_persistence::EventType;

pub(crate) const OUTBOX_TELL_REQUESTED: EventType =
    EventType::from_str("nitinol.saga.outbox.tell_requested");
pub(crate) const OUTBOX_TELL_ACKED: EventType =
    EventType::from_str("nitinol.saga.outbox.tell_acked");
pub(crate) const OUTBOX_TELL_FAILED: EventType =
    EventType::from_str("nitinol.saga.outbox.tell_failed");
pub(crate) const OUTBOX_SCHEDULED: EventType = EventType::from_str("nitinol.saga.outbox.scheduled");
