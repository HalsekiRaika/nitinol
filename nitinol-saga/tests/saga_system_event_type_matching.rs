//! Reading a stored `event_type` back as "is this the framework record that
//! declared it?" — the `SystemEvent` side of the question
//! `nitinol-eventsource/tests/event_type_matching.rs` pins for user events.
//!
//! A framework record reaches the `event_type` column the same way a user's
//! event does, so the same one question has to answer for it. `DeadLetterEvent`
//! is the framework record this crate exports, and it is what the question can
//! be asked of from outside the crate.
//!
//! Its siblings — `nitinol.saga.outbox`, `nitinol.saga.schedule`,
//! `nitinol.saga.dead_letter_disposition` and
//! `nitinol.saga.consumed_position` — are named here as the stored strings
//! they are, because the types declaring them are crate-internal. Those
//! strings are the wire contract a stored row carries, so spelling them out is
//! naming an external contract rather than copying an internal definition.

use nitinol_eventsource::MatchesEventType;
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::{EventType, TypeName};
use nitinol_saga::DeadLetterEvent;

/// `Family::new` refuses the reserved namespace, so a test that has to name a
/// framework record goes through the same constructor the framework does.
fn saga_record(type_name: &'static str) -> EventType {
    EventType::new(reserved_family("nitinol.saga"), TypeName::new(type_name))
}

/// Given the identity a stored dead letter carries, When it is matched against
/// the `SystemEvent` that declared it, Then it answers yes — the same answer
/// the user-event side gives, reached through the same call.
#[test]
fn a_stored_event_type_matches_the_system_event_that_declares_it() {
    let stored = saga_record("dead_letter");

    assert!(
        stored.matches::<DeadLetterEvent>(),
        "`{stored}` is the identity `DeadLetterEvent` declares, so it must match it"
    );
}

/// Given the identity of each of the dead letter's sibling framework records,
/// When each is matched against `DeadLetterEvent`, Then none of them match.
///
/// This is what keeps a settlement marker, an outbox marker, a timer marker
/// and a consumed-position record from being read as dead letters: they share
/// the reserved family, and the family alone is not what a record is
/// recognised by.
#[test]
fn the_sibling_framework_declarations_do_not_match() {
    for sibling in [
        "outbox",
        "schedule",
        "dead_letter_disposition",
        "consumed_position",
    ] {
        let stored = saga_record(sibling);

        assert!(
            !stored.matches::<DeadLetterEvent>(),
            "`{stored}` is a sibling declaration and must not be recognised as a dead letter"
        );
    }
}

/// A row that belongs to no framework declaration at all belongs to the domain,
/// and is not a dead letter either.
#[test]
fn a_domain_declaration_does_not_match() {
    let stored = EventType::new(
        nitinol_persistence::Family::new("shop.order"),
        TypeName::new("OrderPlaced"),
    );

    assert!(
        !stored.matches::<DeadLetterEvent>(),
        "`{stored}` is a domain declaration and must not be recognised as a dead letter"
    );
}
