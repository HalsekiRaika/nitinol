use std::collections::HashMap;

use nitinol_persistence::EventType;

/// The event types a store may restore, looked up by the canonical path a row
/// carries.
///
/// The path a type is stored under is `event_type.to_path().to_string()`, which
/// is the encoding both sides of the column agree on.
#[derive(Debug, Clone, Default)]
pub struct EventTypeRegistry {
    by_path: HashMap<String, EventType>,
}

impl EventTypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `event_type`, returning whatever was registered under its path
    /// before.
    ///
    /// Two different types can share one canonical path — a struct event `C` in
    /// family `a.b` and arm `C` of type `b` in family `a` both read back as
    /// `a.b.C` — so the displaced value is handed back rather than dropped,
    /// leaving a caller that registered both able to notice.
    pub fn register(&mut self, event_type: EventType) -> Option<EventType> {
        self.by_path
            .insert(event_type.to_path().to_string(), event_type)
    }

    pub fn resolve(&self, path: &str) -> Option<EventType> {
        self.by_path.get(path).copied()
    }
}

impl FromIterator<EventType> for EventTypeRegistry {
    fn from_iter<I: IntoIterator<Item = EventType>>(event_types: I) -> Self {
        Self {
            by_path: event_types
                .into_iter()
                .map(|event_type| (event_type.to_path().to_string(), event_type))
                .collect(),
        }
    }
}

/// A stored row carries a path no registry entry answers for.
///
/// Restoring it as some other type would misroute it and skipping it would hide
/// it from every reader, so the row's load fails and the path it carried is
/// named.
#[derive(Debug, thiserror::Error)]
#[error("no event type is registered under the path {path:?}")]
pub struct UnknownEventType {
    pub path: String,
}
