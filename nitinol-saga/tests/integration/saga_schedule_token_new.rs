//! Building a [`ScheduleToken`].
//!
//! A token is the pair `(saga_id, name)` and nothing else, so the pair is
//! settled in one place rather than written out again wherever a timer is
//! addressed — registration, cancellation, firing and replay each address the
//! same timer, and a pair assembled separately at each is a pair that can
//! disagree.
//!
//! `saga_schedule_token.rs` states what the pair *means*: two tokens are the
//! same timer only when both halves match. This file states that the
//! constructor produces exactly that pair, which is what makes replacing a
//! hand-written pair with a call to it a change of nothing observable.

use nitinol_saga::{SagaId, ScheduleToken, TimerName};

/// Given a saga id and a timer name, When a token is constructed from them,
/// Then it carries both and is indistinguishable from the pair written out by
/// hand.
#[test]
fn a_constructed_token_is_the_pair_it_was_given() {
    let saga_id = SagaId::new("order-42");
    let name = TimerName::new("reminder");

    let built = ScheduleToken::new(saga_id.clone(), name.clone());

    assert_eq!(
        built.saga_id, saga_id,
        "the token must carry the SagaId it was built from"
    );
    assert_eq!(
        built.name, name,
        "the token must carry the TimerName it was built from"
    );
    assert_eq!(
        built,
        ScheduleToken { saga_id, name },
        "a constructed token must equal the pair written out by hand, so every site that \
         wrote the pair itself can go through the constructor and address the same timer"
    );
}

/// Given two constructions that differ in one half, When the tokens are
/// compared, Then they are distinct — the constructor settles a pair rather
/// than collapsing one half into the other.
#[test]
fn tokens_constructed_from_different_pairs_stay_distinct() {
    let base = ScheduleToken::new(SagaId::new("order-1"), TimerName::new("reminder"));
    let other_saga = ScheduleToken::new(SagaId::new("order-2"), TimerName::new("reminder"));
    let other_name = ScheduleToken::new(SagaId::new("order-1"), TimerName::new("deadline"));

    assert_ne!(
        base, other_saga,
        "the same timer name in a different saga must be a distinct timer"
    );
    assert_ne!(
        base, other_name,
        "a different timer name in the same saga must be a distinct timer"
    );
}
