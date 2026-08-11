//! Builder panic tests for the `SagaEffect` ADT.
//!
//! `with_tells` / `with_schedules` are defined only on the `Persist` branch.
//! Calling them on `None` / `End` / `Sequence` is a Builder contract
//! violation and must panic to surface the misuse at the earliest possible
//! moment (no silent identity / silent Persist-wrapping).

#[path = "common/helpers.rs"]
mod common;

use std::time::Duration;

use bytes::Bytes;
use nitinol_saga::{SagaEffect, ScheduleSpec, TimerName};

/// Build a `ScheduleSpec` with an empty payload for panic-path tests.
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

#[tokio::test]
#[should_panic]
async fn with_tells_on_none_panics() {
    let intent = common::make_tell_intent().await;
    let _ = SagaEffect::<u32>::empty().with_tells(vec![intent]);
}

#[tokio::test]
#[should_panic]
async fn with_tells_on_end_panics() {
    let intent = common::make_tell_intent().await;
    let _ = SagaEffect::<u32>::end().with_tells(vec![intent]);
}

#[tokio::test]
#[should_panic]
async fn with_tells_on_sequence_panics() {
    let intent = common::make_tell_intent().await;
    let _ = sequence().with_tells(vec![intent]);
}

#[tokio::test]
#[should_panic]
async fn with_tells_on_cancel_schedule_panics() {
    let intent = common::make_tell_intent().await;
    let cancel: SagaEffect<u32> = SagaEffect::cancel_schedule(TimerName::new("x"));
    let _ = cancel.with_tells(vec![intent]);
}

#[test]
#[should_panic]
fn with_schedules_on_none_panics() {
    let _ = SagaEffect::<u32>::empty().with_schedules(vec![spec()]);
}

#[test]
#[should_panic]
fn with_schedules_on_end_panics() {
    let _ = SagaEffect::<u32>::end().with_schedules(vec![spec()]);
}

#[test]
#[should_panic]
fn with_schedules_on_sequence_panics() {
    let _ = sequence().with_schedules(vec![spec()]);
}

// The `with_schedules` × `CancelSchedule` pair is pinned by
// `with_schedules_on_cancel_variant_panics` in the schedule-effect tests, next
// to the rest of the `CancelSchedule` contract; duplicating it here would give
// the same assertion two homes.
