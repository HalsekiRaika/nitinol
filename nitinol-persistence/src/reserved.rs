//! The `nitinol` reserved namespace — one law spanning two name spaces.
//!
//! The framework persists its own records beside the ones an application
//! writes, and it names them from a single root: `nitinol`.  That claim covers
//! **both** name spaces a nitinol deployment has:
//!
//! | space | what carries the name | how the law is enforced |
//! |---|---|---|
//! | stream keys | [`AggregateId`](crate::AggregateId), [`ProjectionId`](crate::ProjectionId), `nitinol_saga::SagaId` — runtime strings | [`reject_reserved_id`] panics |
//! | event types | the `family` of an [`EventType`](crate::EventType) — a compile-time literal | [`Family::new`](crate::Family::new) panics while the constant holding it is evaluated |
//!
//! The two differ only in *when* the name is known, and therefore in what a
//! rejection can be.  The law itself — the root, and the boundary that decides
//! membership — lives next to [`Family`] in
//! [`crate::event_type`](crate::event_type), because `Family::new` is the
//! compile-time enforcement point that needs it in the same evaluation.  This
//! module re-exports [`RESERVED_NAMESPACE`] and [`is_within_reserved_namespace`]
//! so callers can keep reaching them through `nitinol_persistence::reserved`,
//! and it owns the runtime-string enforcement point, [`reject_reserved_id`].
//!
//! # The framework's own records
//!
//! The framework persists records under the names it reserved, so it needs a
//! way past the refusal.  That way is
//! `nitinol_persistence::__private::{reserved_family, reserved_event_registry_entry}`
//! — `__private` because it is not a user surface: it exists so the records
//! nitinol writes can carry the names nitinol reserved, and for nothing else.

pub use crate::event_type::{is_within_reserved_namespace, RESERVED_NAMESPACE};

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

    /// Segment-boundary membership itself is covered next to its definition
    /// in `crate::event_type`; this module only owns `reject_reserved_id`, so
    /// its test covers that runtime-string enforcement point.
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
