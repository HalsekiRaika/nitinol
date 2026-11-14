use crate::event_type::{is_within_reserved_namespace, EventType};

/// The canonical path one declaration is written down as.
///
/// A declaration is a single type, and a row of it carries that type's one
/// path — which arm of an enum the row holds is the payload's to say. The entry
/// therefore carries one event type; the slice shape is what lets a subsystem
/// declare the several types it owns from one place.
///
/// The entry is how a declaration tells the machinery its path: the machinery
/// gathers every entry a binary links (the `EVENT_REGISTRY` slice
/// `nitinol-persistence` owns) and restores a stored row's declaration from the
/// path the row carries. What it reads is exactly what the entry was built
/// with, through
/// [`declared_by`](Self::declared_by) and [`event_types`](Self::event_types);
/// building one stays with [`new`](Self::new), which refuses the reserved
/// namespace.
///
/// `declared_by` names the declaring type well enough for a caller to go and
/// find it. Nothing resolves against it; it is what a collision is reported
/// with, and collection happens at link time, so without it the two definitions
/// to rename are not otherwise in front of the caller.
#[derive(Debug)]
pub struct EventRegistryEntry {
    declared_by: &'static str,
    event_types: &'static [EventType],
}

impl EventRegistryEntry {
    /// An entry for declarations outside the `nitinol` reserved namespace.
    ///
    /// # Panics
    ///
    /// A declaration reaches the `event_type` column through its entry as much
    /// as through its `EVENT_TYPE`, so the namespace is refused here too —
    /// while the `static` holding the entry is evaluated, which is a compile
    /// error rather than a condition a running deployment discovers.
    pub const fn new(declared_by: &'static str, event_types: &'static [EventType]) -> Self {
        let mut nth = 0;
        while nth < event_types.len() {
            if is_within_reserved_namespace(event_types[nth].family().as_str()) {
                panic!(
                    "an entry claiming an event type within the `nitinol` reserved namespace \
                     belongs to framework-owned records; name the family outside it"
                );
            }
            nth += 1;
        }
        Self {
            declared_by,
            event_types,
        }
    }

    /// The same entry without the namespace refusal, for the framework's own
    /// records.
    const fn reserved(declared_by: &'static str, event_types: &'static [EventType]) -> Self {
        Self {
            declared_by,
            event_types,
        }
    }

    /// The declaring type this entry was built for.
    pub const fn declared_by(&self) -> &'static str {
        self.declared_by
    }

    /// The event types this entry claims.
    pub const fn event_types(&self) -> &'static [EventType] {
        self.event_types
    }
}

/// Build an [`EventRegistryEntry`] claiming event types inside the `nitinol`
/// reserved namespace.
///
/// The counterpart of
/// [`reserved_family`](crate::event_type::reserved_family): a framework record
/// is collected the way a user declaration is, and this is what lets its entry
/// name the family [`EventRegistryEntry::new`] refuses.
///
/// Reachable only through this crate's `__private`, which `nitinol-persistence`
/// re-exports in its own; it is not a user surface.
pub const fn reserved_event_registry_entry(
    declared_by: &'static str,
    event_types: &'static [EventType],
) -> EventRegistryEntry {
    EventRegistryEntry::reserved(declared_by, event_types)
}
