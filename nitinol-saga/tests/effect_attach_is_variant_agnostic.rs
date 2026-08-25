//! Attaching a tell or a schedule to an effect is independent of the effect's
//! variant.
//!
//! `tell_intent` / `schedule_spec` build their own `Persist` branch, so the
//! thing they are attached to is reached through `combine` — the Monoid
//! operation, which is total.  There is no receiver whose runtime variant can
//! make an attachment illegal, and therefore no builder call that panics
//! because of one.
//!
//! Each case below is a composition the removed `with_tells` / `with_schedules`
//! setters rejected with a `panic!`.  They assert the resulting shape rather
//! than merely "no panic": a test that only avoids panicking is satisfied by
//! any implementation at all, whereas the shape distinguishes a real
//! composition from one that silently drops the attachment.

#[path = "common/helpers.rs"]
mod common;
use common::{shape_of, Shape};

use std::time::Duration;

use bytes::Bytes;
use nitinol_saga::{SagaEffect, ScheduleSpec, TimerName};

/// Build a `ScheduleSpec` with an empty payload.
fn spec() -> ScheduleSpec {
    ScheduleSpec {
        name: TimerName::new("x"),
        after: Duration::from_secs(1),
        payload: Bytes::new(),
    }
}

/// A genuine multi-leaf `Sequence`.
///
/// `Persist.combine(Persist)` is *not* usable here: an adjacent
/// `Persist × Persist` pair folds into a single `Persist`, which would make
/// these tests exercise the `Persist` branch instead of the `Sequence` one.
/// `End` is never absorbed into a `Persist`, so the junction stays a Sequence.
fn sequence() -> SagaEffect<u32> {
    SagaEffect::persist(1u32).combine(SagaEffect::end())
}

fn persist_leaf() -> Shape<u32> {
    Shape::Persist {
        events: vec![1u32],
        tells: 0,
        schedules: vec![],
    }
}

fn tell_leaf() -> Shape<u32> {
    Shape::Persist {
        events: vec![],
        tells: 1,
        schedules: vec![],
    }
}

fn schedule_leaf() -> Shape<u32> {
    Shape::Persist {
        events: vec![],
        tells: 0,
        schedules: vec![spec()],
    }
}

#[tokio::test]
async fn tell_intent_combined_onto_none_yields_the_tell_branch_alone() {
    let intent = common::make_tell_intent().await;

    let effect = SagaEffect::<u32>::empty().combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        tell_leaf(),
        "None is the Monoid identity, so attaching a tell to it must yield the tell \
         branch unchanged — the removed with_tells panicked on this receiver instead"
    );
}

#[tokio::test]
async fn tell_intent_combined_after_end_stays_behind_the_terminator() {
    let intent = common::make_tell_intent().await;

    let effect = SagaEffect::<u32>::end().combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![Shape::End, tell_leaf()]),
        "End is a hard boundary: the tell must sit after it as its own leaf rather \
         than being folded into it or rejected"
    );
}

#[tokio::test]
async fn tell_intent_combined_after_cancel_schedule_stays_its_own_leaf() {
    let intent = common::make_tell_intent().await;

    let cancel: SagaEffect<u32> = SagaEffect::cancel_schedule(TimerName::new("x"));
    let effect = cancel.combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![Shape::CancelSchedule, tell_leaf()]),
        "CancelSchedule carries its own interpretation and is never absorbed into a \
         Persist, so the tell must follow it as a separate leaf"
    );
}

#[tokio::test]
async fn tell_intent_combined_onto_a_sequence_appends_flatly() {
    let intent = common::make_tell_intent().await;

    let effect = sequence().combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![persist_leaf(), Shape::End, tell_leaf()]),
        "attaching a tell to a Sequence must append it to the flat leaf list without \
         nesting — the removed with_tells rejected every Sequence receiver"
    );
}

#[test]
fn schedule_spec_combined_onto_none_yields_the_schedule_branch_alone() {
    let effect = SagaEffect::<u32>::empty().combine(SagaEffect::schedule_spec(spec()));

    assert_eq!(
        shape_of(&effect),
        schedule_leaf(),
        "None is the Monoid identity, so attaching a schedule to it must yield the \
         schedule branch unchanged"
    );
}

#[test]
fn schedule_spec_combined_after_end_stays_behind_the_terminator() {
    let effect = SagaEffect::<u32>::end().combine(SagaEffect::schedule_spec(spec()));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![Shape::End, schedule_leaf()]),
        "End is a hard boundary: the schedule must sit after it as its own leaf"
    );
}

#[test]
fn schedule_spec_combined_after_cancel_schedule_stays_its_own_leaf() {
    let cancel: SagaEffect<u32> = SagaEffect::cancel_schedule(TimerName::new("x"));
    let effect = cancel.combine(SagaEffect::schedule_spec(spec()));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![Shape::CancelSchedule, schedule_leaf()]),
        "a cancel followed by a new schedule is a legitimate saga intent and must \
         compose, not panic"
    );
}

#[test]
fn schedule_spec_combined_onto_a_sequence_appends_flatly() {
    let effect = sequence().combine(SagaEffect::schedule_spec(spec()));

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![persist_leaf(), Shape::End, schedule_leaf()]),
        "attaching a schedule to a Sequence must append it to the flat leaf list \
         without nesting"
    );
}
