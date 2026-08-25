//! Constructor / Builder tests for the `SagaEffect` ADT.
//!
//! Covers the public surface:
//! - `empty()` / `end()` / `persist()` / `persist_all()` / `tell()`
//! - `tell_intent(..)` / `schedule_spec(..)`
//! - `then_end()`
//!
//! Every constructor here builds a `Persist` branch from scratch, so composing
//! them is `combine`'s job — there is no receiver whose variant could make an
//! attachment illegal.  That the attachment is variant-agnostic is pinned in
//! `effect_attach_is_variant_agnostic.rs`.

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
async fn tell_intent_returns_persist_carrying_only_that_intent() {
    let intent = common::make_tell_intent().await;

    let effect = SagaEffect::<u32>::tell_intent(intent);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            tells: 1,
            schedules: vec![],
        },
        "tell_intent(i) must return Persist {{ events: [], tells: [i], schedules: [] }} — \
         a pre-built TellIntent must reach the Outbox-atomic path without going through \
         SagaEffect::tell's serde serialization, which a crash-restart-less intent cannot use"
    );
}

#[test]
fn schedule_spec_returns_persist_carrying_only_that_spec() {
    let s = spec("raw", Duration::from_secs(30));

    let effect = SagaEffect::<u32>::schedule_spec(s.clone());

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![],
            tells: 0,
            schedules: vec![s],
        },
        "schedule_spec(s) must return Persist {{ events: [], tells: [], schedules: [s] }} — \
         a spec with raw payload bytes must reach the batch without going through \
         SagaEffect::schedule's serde serialization"
    );
}

#[tokio::test]
async fn persist_combined_with_tell_intent_yields_one_persist() {
    let intent = common::make_tell_intent().await;

    let effect = SagaEffect::persist(7u32).combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![7u32],
            tells: 1,
            schedules: vec![],
        },
        "persist(e).combine(tell_intent(i)) must fold into ONE Persist so the event and \
         its TellRequested marker share a single atomic append batch"
    );
}

#[test]
fn persist_combined_with_schedule_spec_yields_one_persist() {
    let s = spec("deadline", Duration::from_secs(60));

    let effect = SagaEffect::persist(13u32).combine(SagaEffect::schedule_spec(s.clone()));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![13u32],
            tells: 0,
            schedules: vec![s],
        },
        "persist(e).combine(schedule_spec(s)) must fold into ONE Persist so the event and \
         its scheduled marker share a single atomic append batch"
    );
}

#[tokio::test]
async fn combining_two_tell_intents_concatenates_rather_than_replaces() {
    // Attachment is `combine`, and `combine` concatenates.  The removed
    // `with_tells` was a *set* — a second call dropped the first intent on the
    // floor.  Concatenation is what makes multiple attachments expressible
    // without a second composition mechanism.
    let first = common::make_tell_intent().await;
    let second = common::make_tell_intent().await;

    let effect = SagaEffect::persist(1u32)
        .combine(SagaEffect::tell_intent(first))
        .combine(SagaEffect::tell_intent(second));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32],
            tells: 2,
            schedules: vec![],
        },
        "chained tell_intent combines must keep BOTH intents; dropping the earlier one \
         would silently lose a saga's dispatch intent"
    );
}

#[test]
fn combining_two_schedule_specs_concatenates_in_left_to_right_order() {
    let a = spec("a", Duration::from_secs(30));
    let b = spec("b", Duration::from_secs(60));

    let effect = SagaEffect::persist(2u32)
        .combine(SagaEffect::schedule_spec(a.clone()))
        .combine(SagaEffect::schedule_spec(b.clone()));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![2u32],
            tells: 0,
            schedules: vec![a, b],
        },
        "chained schedule_spec combines must keep both specs in left-to-right order"
    );
}

#[tokio::test]
async fn tell_intent_and_schedule_spec_combine_onto_the_same_persist() {
    let intent = common::make_tell_intent().await;
    let s = spec("reminder", Duration::from_secs(5));

    let effect = SagaEffect::persist(99u32)
        .combine(SagaEffect::tell_intent(intent))
        .combine(SagaEffect::schedule_spec(s.clone()));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![99u32],
            tells: 1,
            schedules: vec![s],
        },
        "an event, a tell and a schedule attached through combine must land on ONE \
         Persist branch — all three categories share the single atomic batch"
    );
}

#[tokio::test]
async fn merged_persist_pair_combined_with_tell_intent_stays_one_persist() {
    // A pair of `Persist` branches already folded into one by the junction
    // merge must still accept a tell without splitting the batch: the
    // observable contract is that the result stays a single `Persist`.
    let intent = common::make_tell_intent().await;

    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let effect = merged.combine(SagaEffect::tell_intent(intent));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2],
            tells: 1,
            schedules: vec![],
        },
        "persist(1).combine(persist(2)).combine(tell_intent(i)) must stay ONE Persist; \
         folding the pair into a Sequence instead would split the tell marker out of \
         the events' append batch"
    );
}

#[test]
fn merged_persist_pair_combined_with_schedule_spec_stays_one_persist() {
    let s = spec("after-merge", Duration::from_secs(3));

    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let effect = merged.combine(SagaEffect::schedule_spec(s.clone()));

    assert_eq!(
        shape_of(&effect),
        Shape::Persist {
            events: vec![1u32, 2],
            tells: 0,
            schedules: vec![s],
        },
        "persist(1).combine(persist(2)).combine(schedule_spec(s)) must stay ONE Persist, \
         keeping the merged events alongside the scheduled marker"
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
