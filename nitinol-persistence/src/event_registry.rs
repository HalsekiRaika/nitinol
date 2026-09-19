use std::collections::{BTreeMap, HashMap};
use std::fmt;

use linkme::distributed_slice;

use crate::event_type::EventType;
use crate::reserved::is_within_reserved_namespace;

/// The canonical path one declaration is written down as.
///
/// A declaration is a single type, and a row of it carries that type's one
/// path — which arm of an enum the row holds is the payload's to say. The entry
/// therefore carries one event type; the slice shape is what lets a subsystem
/// declare the several types it owns from one place.
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
}

/// Build an [`EventRegistryEntry`] claiming event types inside the `nitinol`
/// reserved namespace.
///
/// The counterpart of
/// [`reserved_family`](crate::event_type::reserved_family): a framework record
/// is collected the way a user declaration is, and this is what lets its entry
/// name the family [`EventRegistryEntry::new`] refuses.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface.
pub const fn reserved_event_registry_entry(
    declared_by: &'static str,
    event_types: &'static [EventType],
) -> EventRegistryEntry {
    EventRegistryEntry::reserved(declared_by, event_types)
}

/// Where every declaration in a binary places its entry.
///
/// `#[derive(Event)]` writes the placement for a type it expands over; a
/// hand-written `impl Event` or `SystemEvent` places the same shape itself:
///
/// ```
/// use linkme::distributed_slice;
/// use nitinol_persistence::{
///     EventRegistry, EventRegistryEntry, EventType, Family, TypeName, EVENT_REGISTRY,
/// };
///
/// const OPENED: EventType = EventType::new(Family::new("shop"), TypeName::new("Opened"));
///
/// #[distributed_slice(EVENT_REGISTRY)]
/// static OPENED_ENTRY: EventRegistryEntry =
///     EventRegistryEntry::new(concat!(module_path!(), "::Opened"), &[OPENED]);
///
/// let registry = EventRegistry::from_linked().expect("distinct canonical paths");
/// assert_eq!(registry.resolve("shop.Opened"), Some(OPENED));
/// ```
///
/// # What link-time collection does not reach
///
/// The slice is filled by the linker, and the linker keeps what the binary
/// links. A crate no item of the binary references may be dropped from the
/// link, and its entries go with it — silently, because nothing at runtime can
/// tell a crate that was never linked from a crate that declared nothing. A row
/// carrying one of the dropped paths then fails its load as an unknown event
/// type rather than being restored. A binary that depends on a crate purely for
/// its event declarations has to reference something of that crate for those
/// declarations to be collected.
///
/// Collection is native-only (servers, Tauri). It rests on the linker gathering
/// statics that crates placed into one named region of the binary, which WASM
/// targets do not provide.
#[distributed_slice]
pub static EVENT_REGISTRY: [EventRegistryEntry];

/// The event types a store may restore, looked up by the canonical path a row
/// carries.
///
/// The path a type is stored under is `event_type.to_path().to_string()`, which
/// is the encoding both sides of the column agree on.
#[derive(Debug)]
pub struct EventRegistry {
    by_path: HashMap<String, EventType>,
}

impl EventRegistry {
    /// The registry of everything this binary linked, with nothing registered
    /// by hand.
    ///
    /// What it answers for is what reached [`EVENT_REGISTRY`]; see that slice
    /// for what link-time collection does not reach.
    pub fn from_linked() -> Result<Self, EventRegistryError> {
        Self::from_entries(EVENT_REGISTRY.iter())
    }

    /// The registry of `entries`, whatever gathered them.
    ///
    /// One canonical path answers for one event type, so an entry claiming a
    /// path another entry already claimed fails the build outright: two types
    /// declaring one family and one type name between them are written down as
    /// the same string, and serving one of them would restore the other's rows
    /// as the wrong type. A declaration collected twice fails the same way —
    /// which of the two copies the linker placed last would otherwise decide
    /// what the registry answers.
    pub fn from_entries<'a, I>(entries: I) -> Result<Self, EventRegistryError>
    where
        I: IntoIterator<Item = &'a EventRegistryEntry>,
    {
        // Ordered by path so that a refusal reads the same from one build to
        // the next: the order entries reach here in is the linker's, which
        // nothing in the source fixes.
        let mut claims: BTreeMap<String, Claim> = BTreeMap::new();
        for entry in entries {
            for event_type in entry.event_types {
                claims
                    .entry(event_type.to_path().to_string())
                    .or_insert_with(|| Claim {
                        event_type: *event_type,
                        declared_by: Vec::new(),
                    })
                    .declared_by
                    .push(entry.declared_by);
            }
        }

        let mut by_path = HashMap::with_capacity(claims.len());
        let mut collisions = Vec::new();
        for (path, claim) in claims {
            // Every contested path is reported, not only the first, so a caller
            // fixes them in one pass rather than one build per collision.
            if claim.declared_by.len() > 1 {
                let mut declared_by = claim.declared_by;
                declared_by.sort_unstable();
                collisions.push(Collision { path, declared_by });
                continue;
            }
            by_path.insert(path, claim.event_type);
        }

        if !collisions.is_empty() {
            return Err(EventRegistryError { collisions });
        }

        Ok(Self { by_path })
    }

    pub fn resolve(&self, path: &str) -> Option<EventType> {
        self.by_path.get(path).copied()
    }
}

/// What has been claimed for one canonical path while entries are being read.
struct Claim {
    event_type: EventType,
    declared_by: Vec<&'static str>,
}

/// One canonical path, and every declaration that claimed it.
#[derive(Debug)]
struct Collision {
    path: String,
    declared_by: Vec<&'static str>,
}

/// A registry could not be built from the entries it was given.
#[derive(Debug)]
pub struct EventRegistryError {
    collisions: Vec<Collision>,
}

impl fmt::Display for EventRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "a canonical path answers for one event type, and these were claimed more than once: ",
        )?;
        for (nth, collision) in self.collisions.iter().enumerate() {
            if nth > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{:?} claimed by ", collision.path)?;
            for (nth, declared_by) in collision.declared_by.iter().enumerate() {
                if nth > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{declared_by:?}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for EventRegistryError {}
