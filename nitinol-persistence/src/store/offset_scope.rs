use crate::reserved::{reject_reserved_id, RESERVED_NAMESPACE};

/// The key one event consumer's position is filed under.
///
/// An [`OffsetStore`](crate::store::OffsetStore) holds the positions of more
/// than one kind of consumer — a projection, a partition of a delivery host, a
/// saga attachment yet to be written — and the rendered string below is the
/// whole of what such a store is told. A consumer builds its scope once, when
/// it starts, and holds it for as long as it runs.
///
/// There are two ways to one, and no third: a deployment names its own, or the
/// framework names its own.
///
/// # Keys that cannot meet
///
/// Two kinds of consumer sharing one table must not be able to render one key,
/// or one would read the other's position as its own. Here is why they cannot:
///
/// * [`new`](Self::new) renders the name **verbatim** and refuses any name
///   inside the [reserved namespace](crate::reserved), the same way every
///   user-facing identifier constructor in the framework does.
/// * [`framework_owned`](Self::framework_owned) renders **inside** that
///   namespace, always.
///
/// So the two key spaces are disjoint by construction rather than by a
/// convention a caller follows: a key the framework renders is one a deployment
/// is refused outright, and sharing a table is what is left once the keys cannot
/// collide.
///
/// # Why the rendering is injective
///
/// A framework-owned key carries three elements — the part of the framework the
/// consumer belongs to, the name it runs under, and which partition of it this
/// is — and no two triples may render one key, whatever the name contains. The
/// rendering is `nitinol.{part}.{name}.{partition}`, and the three elements are
/// recovered from it like this:
///
/// 1. the reserved root is a fixed prefix, so what follows it is the rest;
/// 2. up to the first separator after the root is the `part`. A part carrying a
///    separator, or an empty one, would make that split ambiguous, so one is
///    refused — a part comes from a literal in a framework crate rather than
///    from data, so it is refused the way every programming error in this crate
///    is;
/// 3. after the **last** separator is the `partition`, as decimal digits;
/// 4. what lies between the two is the `name`, whatever it contains.
///
/// The name is the one element the rendering does not constrain, which is what
/// makes step 4 the load-bearing one: a name carrying separators, or ending in
/// decimal digits, is recovered whole because the two splits that frame it are
/// decided from the outside in. Constraining the name instead — folding its
/// separators, say — would fold two deployments' keys together.
///
/// This argument is stated here and nowhere else. A consumer that renders a
/// framework-owned key calls [`framework_owned`](Self::framework_owned) and owes
/// no argument of its own. `nitinol-persistence/tests/offset_scope.rs` is its
/// executable record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OffsetScope {
    rendered: String,
}

impl OffsetScope {
    /// The scope a deployment names itself, rendered verbatim.
    ///
    /// # Invariants
    ///
    /// The name must lie outside the [reserved namespace](crate::reserved),
    /// which is where the framework renders its own keys. Violating this
    /// panics, the same way [`AggregateId::new`](crate::AggregateId::new) does.
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
    /// # Invariants
    ///
    /// `part` must be non-empty and carry no `.`, because the rendering recovers
    /// it by splitting at the first one. Violating this panics: a part is a
    /// literal, so a malformed one is a programming error rather than a runtime
    /// condition.
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
