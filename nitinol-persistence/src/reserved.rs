//! The `nitinol` reserved namespace — one law spanning two name spaces.
//!
//! The framework persists its own records beside the ones an application
//! writes, and it names them from a single root: `nitinol`.  That claim covers
//! **both** name spaces a nitinol deployment has:
//!
//! | space | what carries the name | how the law is enforced |
//! |---|---|---|
//! | stream keys | [`AggregateId`](crate::AggregateId), [`ProjectionId`](crate::ProjectionId), `nitinol_saga::SagaId` — runtime strings | the constructor panics |
//! | event types | the `family` of an [`EventType`](crate::EventType) — a compile-time literal | `#[derive(Event)]` refuses to expand |
//!
//! The two differ only in *when* the name is known, and therefore in what a
//! rejection can be.  The law itself — the root, and the boundary that decides
//! membership — is this module, and both enforcement points read it from here
//! rather than restating it.
//!
//! # Boundary
//!
//! `nitinol` is reserved as a **path root**, on the same `.`-delimited segment
//! boundary [`MaterializedPath::is_within`](crate::MaterializedPath::is_within)
//! already uses for event types.  `nitinol.saga` is inside the namespace;
//! `nitinolx` is an unrelated name that merely begins with the same letters and
//! stays available to applications.
//!
//! # Hand-written `impl Event`
//!
//! A `family` written by hand — through [`Family::new`](crate::Family) in a
//! manual `impl Event` rather than through the derive — is **not** checked.
//! The framework's own record types are built that way, so the check cannot
//! live in `Family::new` itself.  For hand-written implementations the reserved
//! namespace is a documented contract: do not name a family inside `nitinol`.

/// The path root the framework reserves for its own streams and event
/// families.
///
/// Public because it is the name application code must avoid, and the one a
/// rejection names back.
pub const RESERVED_NAMESPACE: &str = "nitinol";

/// Is `path` the reserved root itself, or a name beneath it?
///
/// Membership is decided on `.`-delimited segment boundaries, so `nitinol.saga`
/// is within the namespace while `nitinolx` and `nitinol-1` are not.  The empty
/// string is outside it: an empty family is an explicit, supported value on the
/// event-type side.
pub fn is_within_reserved_namespace(path: &str) -> bool {
    if path == RESERVED_NAMESPACE {
        return true;
    }
    match path.strip_prefix(RESERVED_NAMESPACE) {
        Some(rest) => rest.starts_with('.'),
        None => false,
    }
}

/// Panic when `id` names something inside the reserved namespace.
///
/// Every identifier constructor in the framework — including the ones in other
/// crates, such as `nitinol_saga::SagaId` — routes its refusal through here, so
/// all of them reject the same names and say so the same way.
///
/// A reserved identifier is a programming error, not a runtime condition: it
/// can only come from a literal or a configuration value, never from data the
/// framework itself produced.  It is therefore refused the way
/// [`Family::new`](crate::Family) refuses a malformed family — by panicking.
pub fn reject_reserved_id(kind: &str, id: &str) {
    assert!(
        !is_within_reserved_namespace(id),
        "{kind} `{id}` is within the `{RESERVED_NAMESPACE}` reserved namespace, \
         which belongs to framework-owned streams"
    );
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn the_rejection_names_the_namespace_and_the_offending_value() {
        let panicked = std::panic::catch_unwind(|| reject_reserved_id("id", "nitinol.saga"))
            .expect_err("a reserved id must be refused");
        let message = panicked
            .downcast_ref::<String>()
            .expect("assert! panics with a formatted String");
        assert!(
            message.contains("reserved namespace") && message.contains("nitinol.saga"),
            "the refusal must be identifiable as a reserved-namespace violation and \
             name the value that violated it, got {message:?}"
        );
    }
}
