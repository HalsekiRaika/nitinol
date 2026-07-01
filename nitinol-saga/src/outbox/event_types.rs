use nitinol_persistence::{EventType, Family, TypeName};

const OUTBOX_FAMILY: Family = Family::new("nitinol.saga.outbox");

pub(crate) const OUTBOX_TELL_REQUESTED: EventType =
    EventType::new(OUTBOX_FAMILY, TypeName::new("tell_requested"));
pub(crate) const OUTBOX_TELL_ACKED: EventType =
    EventType::new(OUTBOX_FAMILY, TypeName::new("tell_acked"));
pub(crate) const OUTBOX_TELL_FAILED: EventType =
    EventType::new(OUTBOX_FAMILY, TypeName::new("tell_failed"));
pub(crate) const OUTBOX_SCHEDULED: EventType =
    EventType::new(OUTBOX_FAMILY, TypeName::new("scheduled"));
