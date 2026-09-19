//! The saga's own records are collected the same way a user's events are.
//!
//! ```gherkin
//!   Scenario: saga + Postgres が動く (the half that needs no database)
//!     Given a binary that links nitinol-saga
//!     When a registry is built from what was linked
//!     Then every canonical path a saga journals resolves, so a load of that
//!          stream restores it rather than failing with UnknownEventType
//! ```
//!
//! The four markers below are `pub(crate)`, so what is named here is the wire
//! form rather than the constant: the canonical path is what a journal column
//! holds and what a registry is asked for, and it is the only part of these
//! records the framework cannot change without changing what an existing
//! journal means.
//!
//! A path missing from this list is not covered by this clause; a path in the
//! list that the saga never writes would make the clause pass on a record no
//! reader ever sees. Both are pinned against the declarations in
//! `outbox/message.rs`, `scheduler/event.rs`, `dead_letter/event.rs` and
//! `dead_letter/disposition.rs`.
//!
//! Each of those is one declaration written down as one path. Which arm of it a
//! row holds is carried by the payload's `oneof`, so no path beneath a marker
//! is ever written — and `no_path_beneath_a_marker_is_collected` pins that a
//! registry answers for none.

use nitinol_persistence::__private::linked_event_registry;
use nitinol_saga::SagaId;

/// Every canonical path the saga writes onto a saga's own stream: one per
/// framework record it declares.
const JOURNALLED_PATHS: &[&str] = &[
    "nitinol.saga.outbox",
    "nitinol.saga.schedule",
    "nitinol.saga.dead_letter",
    "nitinol.saga.dead_letter_disposition",
];

/// Paths the saga wrote before arms were removed from the grammar, and writes
/// no longer. A registry that still answered for one of them would be serving
/// an identity no declaration owns.
const PATHS_NO_DECLARATION_OWNS: &[&str] = &[
    "nitinol.saga.outbox.tell_requested",
    "nitinol.saga.outbox.tell_acked",
    "nitinol.saga.outbox.tell_failed",
    "nitinol.saga.outbox.scheduled",
    "nitinol.saga.outbox.ended",
    "nitinol.saga.schedule.scheduled",
    "nitinol.saga.schedule.cancelled",
    "nitinol.saga.schedule.fired",
    "nitinol.saga.dead_letter.tell_failed",
    "nitinol.saga.dead_letter.handle_failed",
    "nitinol.saga.dead_letter.persist_failed",
    "nitinol.saga.dead_letter.ended_saga_received_message",
    "nitinol.saga.dead_letter.decode_failed",
    "nitinol.saga.dead_letter.scheduled_failed",
    "nitinol.saga.dead_letter.tell_failed_hook_failed",
    "nitinol.saga.dead_letter_disposition.processed",
    "nitinol.saga.dead_letter_disposition.evicted",
];

#[test]
fn every_event_type_a_saga_journals_is_collected_from_the_crate_that_declares_it() {
    // The saga crate has to be part of this binary for its entries to be
    // collected at all; a test that touched nothing of it would judge an
    // unlinked crate and pass for the wrong reason.
    let _ = SagaId::new("saga.collection.probe");

    let registry = linked_event_registry().expect(
        "a binary linking nitinol-saga and nothing that collides with it builds a registry",
    );

    let missing: Vec<&str> = JOURNALLED_PATHS
        .iter()
        .copied()
        .filter(|path| registry.resolve(path).is_none())
        .collect();

    assert!(
        missing.is_empty(),
        "a saga stream carries these paths, and every load of a stream holding one of them fails \
         with UnknownEventType while the registry does not answer for it: {missing:?}"
    );
}

#[test]
fn a_collected_saga_path_resolves_to_the_event_type_that_is_written_under_it() {
    let _ = SagaId::new("saga.collection.probe");

    let registry = linked_event_registry().expect(
        "a binary linking nitinol-saga and nothing that collides with it builds a registry",
    );

    for path in JOURNALLED_PATHS {
        let restored = registry
            .resolve(path)
            .unwrap_or_else(|| panic!("{path} is journalled by the saga and must be collected"));
        assert_eq!(
            restored.to_path().to_string(),
            *path,
            "a path resolves to the event type stored under it, not to a neighbour: a marker \
             restored as a sibling declaration is a record handed to the wrong reader"
        );
    }
}

#[test]
fn no_path_beneath_a_marker_is_collected() {
    let _ = SagaId::new("saga.collection.probe");

    let registry = linked_event_registry().expect(
        "a binary linking nitinol-saga and nothing that collides with it builds a registry",
    );

    let claimed: Vec<&str> = PATHS_NO_DECLARATION_OWNS
        .iter()
        .copied()
        .filter(|path| registry.resolve(path).is_some())
        .collect();

    assert!(
        claimed.is_empty(),
        "a framework record is one declaration written down as one path, and the arm it holds is \
         carried by its payload; a registry answering for these would give a second identity to a \
         record that has one: {claimed:?}"
    );
}
