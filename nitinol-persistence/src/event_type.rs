//! Structured event identity types: [`Family`], [`TypeName`], [`EventType`].
//!
//! # The grammar a journal's `event_type` column is written in
//!
//! An [`EventType`] is a [`Family`] and a [`TypeName`], and nothing else. A
//! `TypeName` is a single segment by construction, so a canonical path
//! decomposes structurally and without ambiguity: **the last segment is the
//! type name, and every segment before it is the family**. One path, one
//! reading, one declaration.
//!
//! # Design note — why `EventType` does not implement `FromStr`
//!
//! Both components of [`EventType`] are `&'static str`, making it `Copy` and
//! usable in `const` contexts (e.g. `const EVENT_TYPE: EventType = …`).
//! That static-lifetime constraint means it is impossible to reconstruct an
//! `EventType` from a runtime-owned string without leaking memory.
//!
//! For string ↔ component conversions at runtime boundaries, use the two
//! complementary owned types:
//!
//! - **`EventType` → wire string**: `event_type.to_path().to_string()`
//! - **wire string → `MaterializedPath`**: `s.parse::<MaterializedPath>()?`
//! - **`MaterializedPath` → components**: `ParsedEventType::from_path(path)`
//! - **`EventType` → owned components**: `ParsedEventType::from(event_type)`
//!
//! The canonical string produced by `EventType::to_path().to_string()` equals
//! `EventType::Display`, so both sides of a wire column agree on encoding.

use std::fmt;

use crate::materialized_path::MaterializedPath;

/// The path root the framework reserves for its own streams and event
/// families.
///
/// Public because it is the name application code must avoid, and the one a
/// rejection names back.
///
/// Defined here, beside [`Family`], because [`Family::new`] is the
/// compile-time enforcement point: it panics while the `const`/`static`
/// holding a reserved family is evaluated.
/// `nitinol_persistence::reserved` re-exports this for the runtime-string
/// enforcement point, [`reject_reserved_id`](crate::reject_reserved_id).
pub const RESERVED_NAMESPACE: &str = "nitinol";

/// Is `path` the reserved root itself, or a name beneath it?
///
/// Membership is decided on `.`-delimited segment boundaries, so `nitinol.saga`
/// is within the namespace while `nitinolx` and `nitinol-1` are not. The empty
/// string is outside it: an empty family is an explicit, supported value on the
/// event-type side.
///
/// `const` because a family is refused while the `const`/`static` holding it is
/// evaluated; the byte walk is the `.`-boundary rule spelled without the
/// non-`const` string helpers.
pub const fn is_within_reserved_namespace(path: &str) -> bool {
    let path = path.as_bytes();
    let root = RESERVED_NAMESPACE.as_bytes();
    if path.len() < root.len() {
        return false;
    }
    let mut i = 0;
    while i < root.len() {
        if path[i] != root[i] {
            return false;
        }
        i += 1;
    }
    path.len() == root.len() || path[root.len()] == b'.'
}

/// Hierarchical dotted namespace of an event family (e.g. `shop.order.outbox`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Family(&'static str);

impl Family {
    /// Creates a `Family` from a `&'static str`.
    ///
    /// # Invariants
    ///
    /// The empty string is allowed (no namespace prefix). For non-empty values,
    /// every `.`-separated segment must be non-empty: no leading dot, no
    /// trailing dot, and no consecutive dots.
    ///
    /// The family must also lie outside the `nitinol` reserved namespace, which
    /// belongs to the framework's own records.  Violating either rule panics
    /// while the `const`/`static` holding the family is evaluated — a compile
    /// error rather than a condition a running deployment discovers.
    pub const fn new(s: &'static str) -> Self {
        if is_within_reserved_namespace(s) {
            panic!(
                "a family within the `nitinol` reserved namespace belongs to framework-owned \
                 records; name the family outside it"
            );
        }
        Self::well_formed(s)
    }

    /// The shape rule every family obeys, whoever declares it.
    const fn well_formed(s: &'static str) -> Self {
        let bytes = s.as_bytes();
        let len = bytes.len();
        if len == 0 {
            return Self(s);
        }
        if bytes[0] == b'.' {
            panic!("Family must not start with '.'");
        }
        if bytes[len - 1] == b'.' {
            panic!("Family must not end with '.'");
        }
        let mut i = 1;
        while i < len {
            if bytes[i] == b'.' && bytes[i - 1] == b'.' {
                panic!("Family must not contain consecutive dots '..'");
            }
            i += 1;
        }
        Self(s)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }

    /// Hierarchical prefix match on segment boundaries.
    ///
    /// Returns `true` when `self` equals `ancestor` or is one of its
    /// descendants. Matching happens on `.`-delimited segment boundaries, so a
    /// raw string prefix that does not end a segment is rejected
    /// (`shop.orderx` is not within `shop.order`).
    pub fn is_within(&self, ancestor: Family) -> bool {
        if self.0 == ancestor.0 {
            return true;
        }
        match self.0.strip_prefix(ancestor.0) {
            Some(rest) => rest.starts_with('.'),
            None => false,
        }
    }
}

/// Build a [`Family`] inside the `nitinol` reserved namespace.
///
/// The framework persists records of its own, and they carry the names the
/// framework reserved — which is exactly what [`Family::new`] refuses.  This is
/// the one way in, and it waives the namespace rule alone: a malformed family
/// is malformed whoever declares it.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface.
pub const fn reserved_family(s: &'static str) -> Family {
    Family::well_formed(s)
}

/// Type name of an event within its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeName(&'static str);

impl TypeName {
    /// Creates a `TypeName` from a `&'static str`.
    ///
    /// # Invariants
    ///
    /// The string must be non-empty and must not contain `'.'` (a single
    /// path segment with no sub-segments).  Violating this panics at compile
    /// time when called in a `const` context, or at runtime otherwise.
    ///
    /// A dotted type name is what would make the grammar ambiguous: a reader
    /// splits a canonical path on its last segment, so a name spanning two
    /// segments could be read as a deeper family and a shorter name.
    pub const fn new(s: &'static str) -> Self {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            panic!("TypeName must not be empty");
        }
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            if bytes[i] == b'.' {
                panic!("TypeName must not contain '.'");
            }
            i += 1;
        }
        Self(s)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

/// Identity of an event: the family it belongs to and its type name.
///
/// This is the whole of what a stored row's `event_type` column carries. Which
/// arm of an enum event a row holds is carried by the payload — the codec the
/// declaration names is what reads it back — so the column names the
/// declaration and the payload names the case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventType {
    family: Family,
    type_name: TypeName,
}

impl EventType {
    pub const fn new(family: Family, type_name: TypeName) -> Self {
        Self { family, type_name }
    }

    pub const fn family(&self) -> Family {
        self.family
    }

    pub const fn type_name(&self) -> TypeName {
        self.type_name
    }

    pub fn to_path(&self) -> MaterializedPath {
        MaterializedPath::from(self)
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.family.0.is_empty() {
            write!(f, "{}.", self.family.0)?;
        }
        f.write_str(self.type_name.0)
    }
}

// Owned round-trippable projection of EventType components

/// Owned representation of [`EventType`] components for serialisation /
/// deserialisation at wire or storage boundaries.
///
/// Unlike [`EventType`] (which uses `&'static str` and is `Copy` / `const`),
/// `ParsedEventType` holds owned [`String`]s so it can be constructed from a
/// runtime string — for example via [`ParsedEventType::from_path`].
///
/// # Round-trip contract
///
/// ```text
/// EventType::new(family, type_name)
///   → .to_path().to_string()   ← canonical wire string
///   → .parse::<MaterializedPath>()
///   → ParsedEventType::from_path(path)
///   == ParsedEventType::from(event_type)   ✓
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedEventType {
    /// Dot-separated namespace prefix (empty string for top-level events).
    pub family: String,
    /// Single-segment type identifier (no dots).
    pub type_name: String,
}

impl ParsedEventType {
    /// Decompose a [`MaterializedPath`] into the components it was written from.
    ///
    /// The last segment is the `type_name`; all preceding segments are joined
    /// with `.` to form the `family` (empty string when there is only one
    /// segment). A type name is one segment, so this is the only reading a
    /// canonical path has.
    pub fn from_path(path: MaterializedPath) -> Self {
        let mut segments = path.segments;
        // SAFETY: MaterializedPath invariant — at least one segment.
        let type_name = segments
            .pop()
            .expect("MaterializedPath is always non-empty");
        let family = segments.join(".");
        Self { family, type_name }
    }
}

impl From<EventType> for ParsedEventType {
    /// Convert a static-lifetime `EventType` into an owned `ParsedEventType`.
    fn from(event_type: EventType) -> Self {
        Self {
            family: event_type.family().as_str().to_owned(),
            type_name: event_type.type_name().as_str().to_owned(),
        }
    }
}

impl fmt::Display for ParsedEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.family.is_empty() {
            write!(f, "{}.", self.family)?;
        }
        f.write_str(&self.type_name)
    }
}

#[cfg(test)]
mod reserved_namespace_tests {
    use super::*;

    /// The boundary is a segment boundary, not a string prefix — the one thing
    /// a naive `starts_with` implementation would get wrong, in both
    /// directions.
    #[test]
    fn membership_is_decided_on_segment_boundaries() {
        for inside in ["nitinol", "nitinol.saga", "nitinol.saga.dead_letter"] {
            assert!(
                is_within_reserved_namespace(inside),
                "{inside:?} must be inside the reserved namespace"
            );
        }
        for outside in [
            "nitinolx",
            "nitinol-1",
            "nitinol_saga",
            "my.nitinol",
            "order-7",
            "",
        ] {
            assert!(
                !is_within_reserved_namespace(outside),
                "{outside:?} must not be inside the reserved namespace"
            );
        }
    }
}
