use nitinol_persistence::{EventType, Family, TypeName};

pub(crate) const OUTBOX_MARKER: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("outbox"));
