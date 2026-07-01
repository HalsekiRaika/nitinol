use std::fmt;

/// Hierarchical dotted namespace of an event family (e.g. `nitinol.saga.outbox`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Family(&'static str);

impl Family {
    pub const fn new(s: &'static str) -> Self {
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
    pub const fn new(s: &'static str) -> Self {
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
    pub const fn new(s: &'static str) -> Self {
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
}

/// One-way rendering `family.type_name[.variant]` for display and single-column
/// fallback. The family prefix (and its dot) is omitted when the family is
/// empty. Not reversible: persistence and routing must use the components.
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
