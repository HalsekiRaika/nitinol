//! Constructor / Builder tests for the `SagaEffect` ADT.
//!
//! Covers the public surface:
//! - `empty()` / `end()` / `persist()` / `persist_all()` / `tell()`
//! - `with_tells(...)` / `with_schedules(...)`
//! - `then_end()`
//!
//! Panics for misuse of `with_tells` / `with_schedules` on non-`Persist`
//! variants are exercised separately in `effect_builder_panics.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::{shape_of, Shape};

use std::time::Duration;

use bytes::Bytes;
use nitinol_saga::{SagaEffect, ScheduleSpec, TimerName};

/// Build a `ScheduleSpec` from a name and delay with an empty payload.
fn spec(name: &str, after: Duration) -> ScheduleSpec {
    ScheduleSpec {
        name: TimerName::new(name),
        after,
        payload: Bytes::new(),
    }
}

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
fn persist_wraps_single_event_with_empty_tells_and_schedules() {
    let effect = SagaEffect::persist(42u32);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![42u32],
            tells: 0,
            schedules: vec![],
        },
        "persist(e) must return Persist {{ events: [e], tells: [], schedules: [] }}"
    );
}

#[test]
fn persist_all_wraps_multiple_events_with_empty_tells_and_schedules() {
    let effect = SagaEffect::persist_all(vec![1u32, 2, 3]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2, 3],
            tells: 0,
            schedules: vec![],
        },
        "persist_all(vec) must keep events verbatim and start with empty tells/schedules"
    );
}

#[test]
fn persist_all_with_empty_vec_returns_persist_with_no_events() {
    let effect = SagaEffect::<u32>::persist_all(vec![]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            tells: 0,
            schedules: vec![],
        },
        "persist_all([]) must return Persist with empty events — semantically distinct from None"
    );
}

#[tokio::test]
async fn tell_constructor_returns_persist_with_single_tell_intent() {
    let effect = common::make_tell_effect::<u32>().await;

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            tells: 1,
            schedules: vec![],
        },
        "SagaEffect::tell(target, cmd) must return Persist {{ events: [], tells: [1], schedules: [] }} — \
         tells live inside Persist, never as a top-level Tell variant"
    );
}

#[tokio::test]
async fn with_tells_attaches_intents_to_persist_branch() {
    let intent_a = common::make_tell_intent().await;
    let intent_b = common::make_tell_intent().await;

    let effect = SagaEffect::persist(7u32).with_tells(vec![intent_a, intent_b]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![7u32],
            tells: 2,
            schedules: vec![],
        },
        "with_tells(intents) must attach all intents to the Persist branch \
         without dropping its events or schedules"
    );
}

#[tokio::test]
async fn with_tells_replacing_existing_intents_keeps_only_the_new_list() {
    // Builder semantics: with_tells is a *set*, not a *merge*.  Calling it twice
    // keeps only the most recent list — the same semantics SagaContext uses for
    // with_upstream / with_now.
    let first = common::make_tell_intent().await;
    let second_a = common::make_tell_intent().await;
    let second_b = common::make_tell_intent().await;
    let second_c = common::make_tell_intent().await;

    let effect = SagaEffect::persist(1u32)
        .with_tells(vec![first])
        .with_tells(vec![second_a, second_b, second_c]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32],
            tells: 3,
            schedules: vec![],
        },
        "with_tells called twice must keep only the final list (set, not merge)"
    );
}

#[test]
fn with_schedules_attaches_schedules_to_persist_branch() {
    let a = spec("a", Duration::from_secs(30));
    let b = spec("b", Duration::from_secs(60));

    let effect = SagaEffect::persist(13u32).with_schedules(vec![a.clone(), b.clone()]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![13u32],
            tells: 0,
            schedules: vec![a, b],
        },
        "with_schedules(s) must attach the schedules to the Persist branch in order"
    );
}

#[test]
fn with_schedules_replacing_existing_schedules_keeps_only_the_new_list() {
    let old = spec("old", Duration::from_secs(1));
    let new = spec("new", Duration::from_secs(2));

    let effect = SagaEffect::persist(2u32)
        .with_schedules(vec![old])
        .with_schedules(vec![new.clone()]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![2u32],
            tells: 0,
            schedules: vec![new],
        },
        "with_schedules called twice must keep only the final list (set, not merge)"
    );
}

#[tokio::test]
async fn with_tells_and_with_schedules_compose_on_same_persist() {
    let intent = common::make_tell_intent().await;
    let s = spec("reminder", Duration::from_secs(5));

    let effect = SagaEffect::persist(99u32)
        .with_tells(vec![intent])
        .with_schedules(vec![s.clone()]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![99u32],
            tells: 1,
            schedules: vec![s],
        },
        "with_tells and with_schedules must compose on the same Persist branch"
    );
}

#[tokio::test]
async fn merged_persist_pair_is_still_a_valid_with_tells_receiver() {
    let intent = common::make_tell_intent().await;

    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let effect = merged.with_tells(vec![intent]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2],
            tells: 1,
            schedules: vec![],
        },
        "chaining two Persist branches yields a Persist, so with_tells keeps a \
         legal receiver; folding the pair into a Sequence instead would turn this \
         builder call into a panic"
    );
}

#[test]
fn merged_persist_pair_is_still_a_valid_with_schedules_receiver() {
    let s = spec("after-merge", Duration::from_secs(3));

    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let effect = merged.with_schedules(vec![s.clone()]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2],
            tells: 0,
            schedules: vec![s],
        },
        "chaining two Persist branches yields a Persist, so with_schedules keeps a \
         legal receiver and the merged events survive the builder call"
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
                tells: 0,
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
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::cancel_schedule(TimerName::new("t")));
    let effect = seq.then_end();

    assert_eq!(
        shape_of(&effect),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![1u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::CancelSchedule,
            Shape::End,
        ]),
        "Sequence.then_end() must append End to the flat Sequence, not introduce nesting"
    );
}
