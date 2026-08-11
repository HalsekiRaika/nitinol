//! Structured event identity types: [`Family`], [`TypeName`], [`Variant`], [`EventType`].
//!
//! # Design note — why `EventType` does not implement `FromStr`
//!
//! All components of [`EventType`] are `&'static str`, making it `Copy` and
//! usable in `const` contexts (e.g. `const EVENT_TYPE: EventType = …`).
//! That static-lifetime constraint means it is impossible to reconstruct an
//! `EventType` from a runtime-owned string without leaking memory.
//!
//! For string ↔ component conversions at runtime boundaries, use the two
//! complementary owned types:
//!
//! - **`EventType` → wire string**: `event_type.to_path().to_string()`
//! - **wire string → `MaterializedPath`**: `s.parse::<MaterializedPath>()?`
//! - **`MaterializedPath` → components (struct event)**:
//!   `ParsedEventType::from_struct_path(path)`
//! - **`MaterializedPath` → components (enum arm event)**:
//!   `ParsedEventType::try_from_enum_arm_path(path)`
//! - **`EventType` → owned components**: `ParsedEventType::from(event_type)`
//!
//! The canonical string produced by `EventType::to_path().to_string()` equals
//! `EventType::Display`, so both sides of a wire column agree on encoding.

use std::fmt;

use crate::materialized_path::MaterializedPath;

/// Hierarchical dotted namespace of an event family (e.g. `nitinol.saga.outbox`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Family(&'static str);

impl Family {
    /// Creates a `Family` from a `&'static str`.
    ///
    /// # Invariants
    ///
    /// The empty string is allowed (no namespace prefix). For non-empty values,
    /// every `.`-separated segment must be non-empty: no leading dot, no
    /// trailing dot, and no consecutive dots.  Violating this panics at compile
    /// time when called in a `const` context, or at runtime otherwise.
    pub const fn new(s: &'static str) -> Self {
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
    /// (`nitinol.sagax` is not within `nitinol.saga`).
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

/// Enum arm discriminator carried by value-level event identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Variant(&'static str);

impl Variant {
    /// Creates a `Variant` from a `&'static str`.
    ///
    /// # Invariants
    ///
    /// The string must be non-empty and must not contain `'.'` (a single
    /// path segment with no sub-segments).  Violating this panics at compile
    /// time when called in a `const` context, or at runtime otherwise.
    pub const fn new(s: &'static str) -> Self {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            panic!("Variant must not be empty");
        }
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            if bytes[i] == b'.' {
                panic!("Variant must not contain '.'");
            }
            i += 1;
        }
        Self(s)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

/// Variant-free identity of an event, used as a decode-registry / routing key.
///
/// Excludes the variant so a value-level `Some(..)` identity resolves to the
/// same type-registered entry as its `None` type-level counterpart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeKey {
    family: Family,
    type_name: TypeName,
}

/// Structured identity of an event: family, type name, and optional variant.
///
/// `variant` is `None` for struct events (type-level identity) and `Some` for
/// a specific enum arm (value-level identity). All three components participate
/// in `Eq`/`Hash`; routing uses the variant-free [`EventType::type_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventType {
    family: Family,
    type_name: TypeName,
    variant: Option<Variant>,
}

impl EventType {
    pub const fn new(family: Family, type_name: TypeName) -> Self {
        Self {
            family,
            type_name,
            variant: None,
        }
    }

    pub const fn with_variant(family: Family, type_name: TypeName, variant: Variant) -> Self {
        Self {
            family,
            type_name,
            variant: Some(variant),
        }
    }

    pub const fn family(&self) -> Family {
        self.family
    }

    pub const fn type_name(&self) -> TypeName {
        self.type_name
    }

    pub const fn variant(&self) -> Option<Variant> {
        self.variant
    }

    pub const fn type_key(&self) -> TypeKey {
        TypeKey {
            family: self.family,
            type_name: self.type_name,
        }
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
        write!(f, "{}", self.type_name.0)?;
        if let Some(variant) = self.variant {
            write!(f, ".{}", variant.0)?;
        }
        Ok(())
    }
}

// Owned round-trippable projection of EventType components

/// Owned representation of [`EventType`] components for serialisation /
/// deserialisation at wire or storage boundaries.
///
/// Unlike [`EventType`] (which uses `&'static str` and is `Copy` / `const`),
/// `ParsedEventType` holds owned [`String`]s so it can be constructed from a
/// runtime string — for example via [`ParsedEventType::from_struct_path`] or
/// [`ParsedEventType::try_from_enum_arm_path`].
///
/// # Round-trip contract
///
/// For a **struct event** (no variant):
/// ```text
/// EventType::new(family, type_name)
///   → .to_path().to_string()   ← canonical wire string
///   → .parse::<MaterializedPath>()
///   → ParsedEventType::from_struct_path(path)
///   == ParsedEventType::from(event_type)   ✓
/// ```
///
/// For an **enum-arm event** (with variant):
/// ```text
/// EventType::with_variant(family, type_name, variant)
///   → .to_path().to_string()
///   → .parse::<MaterializedPath>()
///   → ParsedEventType::try_from_enum_arm_path(path).unwrap()
///   == ParsedEventType::from(event_type)   ✓
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedEventType {
    /// Dot-separated namespace prefix (empty string for top-level events).
    pub family: String,
    /// Single-segment type identifier (no dots).
    pub type_name: String,
    /// Enum-arm discriminator; `None` for struct events.
    pub variant: Option<String>,
}

impl ParsedEventType {
    /// Decompose a [`MaterializedPath`] as a **struct event** (no variant).
    ///
    /// Convention: the last segment is the `type_name`; all preceding segments
    /// are joined with `.` to form the `family` (empty string when there is
    /// only one segment).
    pub fn from_struct_path(path: MaterializedPath) -> Self {
        let mut segs = path.segments;
        // SAFETY: MaterializedPath invariant — at least one segment.
        let type_name = segs.pop().expect("MaterializedPath is always non-empty");
        let family = segs.join(".");
        Self {
            family,
            type_name,
            variant: None,
        }
    }

    /// Decompose a [`MaterializedPath`] as an **enum-arm event** (with variant).
    ///
    /// Convention: the last segment is the `variant`, the second-to-last is
    /// the `type_name`, and the remaining segments form the `family`.
    ///
    /// Returns `None` when the path has fewer than 2 segments — there is no
    /// room for both a `type_name` and a `variant`.
    pub fn try_from_enum_arm_path(path: MaterializedPath) -> Option<Self> {
        let mut segs = path.segments;
        if segs.len() < 2 {
            return None;
        }
        let variant = segs
            .pop()
            .expect("segment count was checked to be at least 2");
        let type_name = segs
            .pop()
            .expect("segment count was checked to be at least 2");
        let family = segs.join(".");
        Some(Self {
            family,
            type_name,
            variant: Some(variant),
        })
    }
}

impl From<EventType> for ParsedEventType {
    /// Convert a static-lifetime `EventType` into an owned `ParsedEventType`.
    fn from(et: EventType) -> Self {
        Self {
            family: et.family().as_str().to_owned(),
            type_name: et.type_name().as_str().to_owned(),
            variant: et.variant().map(|v| v.as_str().to_owned()),
        }
    }
}

impl fmt::Display for ParsedEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.family.is_empty() {
            write!(f, "{}.", self.family)?;
        }
        write!(f, "{}", self.type_name)?;
        if let Some(ref variant) = self.variant {
            write!(f, ".{}", variant)?;
        }
        Ok(())
    }
}
