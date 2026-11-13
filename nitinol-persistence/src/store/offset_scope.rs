//! The key an event consumer's position is filed under.

use crate::reserved::{reject_reserved_id, RESERVED_NAMESPACE};

/// The key one event consumer's position is filed under.
///
/// The rendered string is the whole of what an
/// [`OffsetStore`](crate::store::OffsetStore) is told, and one store holds the
/// positions of more than one kind of consumer. A consumer builds its scope
/// once, when it starts, and holds it for as long as it runs.
///
/// - [`new`](Self::new) renders the name verbatim and refuses any name inside
///   the [reserved namespace](crate::reserved);
///   [`framework_owned`](Self::framework_owned) always renders inside it. The
///   two key spaces are disjoint, so no consumer reads another's position.
/// - The framework-owned rendering `nitinol.{part}.{name}.{partition}` is
///   injective, whatever `name` contains: `part` is non-empty and carries no
///   `.`, so it ends at the first `.` after the root, and `partition` is the
///   decimal digits after the last `.`; `name` is what lies between.
///
/// `nitinol-persistence/tests/offset_scope.rs` is the executable record of both.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OffsetScope {
    rendered: String,
}

impl OffsetScope {
    /// The scope a deployment names itself, rendered verbatim.
    ///
    /// # Panics
    ///
    /// Panics if `name` is within the [reserved namespace](crate::reserved),
    /// which is where the framework renders its own keys, the same way
    /// [`AggregateId::new`](crate::AggregateId::new) does.
    pub fn new(name: impl Into<String>) -> Self {
        let rendered = name.into();
        reject_reserved_id("offset scope", &rendered);
        Self { rendered }
    }

    /// The scope a part of the framework names for one partition of one of its
    /// own consumers, rendered inside the reserved namespace.
    ///
    /// `part` names the part of the framework the consumer belongs to and comes
    /// from a literal in the crate that owns it — `nitinol-persistence` does not
    /// know what parts there are. `name` is whatever that consumer runs under
    /// and is not constrained. `partition` is which partition of it this is.
    ///
    /// # Panics
    ///
    /// Panics if `part` is empty or contains `.`, because the rendering
    /// recovers it by splitting at the first one. A part is a literal, so a
    /// malformed one is a programming error rather than a runtime condition.
    pub fn framework_owned(part: &'static str, name: &str, partition: usize) -> Self {
        assert!(
            !part.is_empty() && !part.contains('.'),
            "framework-owned offset scope part `{part}` must be non-empty and carry \
             no `.`, because the rendering recovers the part by splitting at the \
             first one"
        );
        Self {
            rendered: format!("{RESERVED_NAMESPACE}.{part}.{name}.{partition}"),
        }
    }

    /// The rendered key, which is the whole of what a store is told.
    pub fn as_str(&self) -> &str {
        &self.rendered
    }
}
