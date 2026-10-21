//! Constructor / Builder tests for the `SagaEffect` ADT.
//!
//! Covers the public surface:
//! - `empty()` / `end()` / `persist()` / `persist_all()`
//! - `then_end()`
//!
//! Every constructor here builds a `Persist` branch from scratch, so composing
//! them is `combine`'s job — there is no receiver whose variant could make an
//! attachment illegal.
//!
//! The schedule-carrying counterparts of these cases are not writable from out
//! here — a `ScheduleSpec` is only built by `Deadline::set`, which needs the
//! component the producer is handed — so they sit beside `combine` in
//! `nitinol-saga/src/effect/helper.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::{shape_of, Shape};

use nitinol_saga::{SagaEffect, TimerName};

#[test]
fn empty_returns_none_variant() {
    let effect = SagaEffect::<()>::empty();

    assert!(
        matches!(effect, SagaEffect::None),
        "empty() must return SagaEffect::None"
    );
}

#[test]
fn end_returns_end_variant() {
    let effect = SagaEffect::<()>::end();

    assert!(
        matches!(effect, SagaEffect::End),
        "end() must return SagaEffect::End"
    );
}

#[test]
fn persist_wraps_single_event_with_no_schedules() {
    let effect = SagaEffect::persist(42u32);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![42u32],
            schedules: vec![],
        },
        "persist(e) must return Persist {{ events: [e], schedules: [] }}"
    );
}

#[test]
fn persist_all_wraps_multiple_events_with_no_schedules() {
    let effect = SagaEffect::persist_all(vec![1u32, 2, 3]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2, 3],
            schedules: vec![],
        },
        "persist_all(vec) must keep events verbatim and start with no schedules"
    );
}

#[test]
fn persist_all_with_empty_vec_returns_persist_with_no_events() {
    let effect = SagaEffect::<u32>::persist_all(vec![]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            schedules: vec![],
        },
        "persist_all([]) must return Persist with empty events — semantically distinct from None"
    );
}

#[test]
fn then_end_on_persist_appends_end_as_sequence_in_order() {
    let effect = SagaEffect::persist(5u32).then_end();

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![5u32],
                schedules: vec![],
            },
            Shape::End,
        ]),
        "then_end() must append End after self via the Monoid combine, preserving order"
    );
}

#[test]
fn then_end_on_none_collapses_to_end() {
    let effect = SagaEffect::<u32>::empty().then_end();

    assert!(
        matches!(effect, SagaEffect::End),
        "None.then_end() must collapse to End via the Monoid identity rule \
         (None.combine(End) == End)"
    );
}

#[test]
fn then_end_on_sequence_appends_end_to_existing_sequence() {
    // `Persist.combine(Persist)` folds into a single `Persist`, so the Sequence
    // premise is built from leaves that never merge.
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::CancelSchedule(TimerName::new("t")));
    let effect = seq.then_end();

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![1u32],
                schedules: vec![],
            },
            Shape::CancelSchedule,
            Shape::End,
        ]),
        "Sequence.then_end() must append End to the flat Sequence, not introduce nesting"
    );
}
