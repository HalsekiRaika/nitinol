//! Builder panic tests for the post-#45 `SagaEffect` ADT.
//!
//! `with_tells` / `with_schedules` are defined only on the `Persist` branch.
//! Per spec, calling them on `None` / `End` / `Sequence` is a Builder contract
//! violation and must panic to surface the misuse at the earliest possible
//! moment (no silent identity / silent Persist-wrapping).

mod common;

use nitinol_saga::SagaEffect;

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
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let _ = seq.with_tells(vec![intent]);
}

#[test]
#[should_panic]
fn with_schedules_on_none_panics() {
    let ts = jiff::Timestamp::from_second(1_700_000_000)
        .expect("constructing a valid jiff::Timestamp must succeed");
    let _ = SagaEffect::<u32>::empty().with_schedules(vec![common::schedule_at_ts(ts)]);
}

#[test]
#[should_panic]
fn with_schedules_on_end_panics() {
    let ts = jiff::Timestamp::from_second(1_700_000_000)
        .expect("constructing a valid jiff::Timestamp must succeed");
    let _ = SagaEffect::<u32>::end().with_schedules(vec![common::schedule_at_ts(ts)]);
}

#[test]
#[should_panic]
fn with_schedules_on_sequence_panics() {
    let ts = jiff::Timestamp::from_second(1_700_000_000)
        .expect("constructing a valid jiff::Timestamp must succeed");
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let _ = seq.with_schedules(vec![common::schedule_at_ts(ts)]);
}
