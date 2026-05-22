mod common;
use common::{shape_of, Shape};

use nitinol_saga::SagaEffect;

#[test]
fn empty_returns_none_variant() {
    let effect = SagaEffect::<()>::empty();

    assert!(
        matches!(effect, SagaEffect::None),
        "empty() must return SagaEffect::None"
    );
}

#[test]
fn persist_wraps_single_event_in_persist_variant() {
    let event = 42u32;
    let effect = SagaEffect::persist(event);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![42u32]),
        "persist(e) must return Persist([e])"
    );
}

#[test]
fn persist_all_wraps_multiple_events_in_persist_variant() {
    let events = vec![1u32, 2, 3];
    let effect = SagaEffect::persist_all(events);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![1u32, 2, 3]),
        "persist_all(vec) must return Persist(vec) verbatim"
    );
}

#[test]
fn persist_all_with_empty_vec_returns_persist_with_no_events() {
    let effect = SagaEffect::<u32>::persist_all(vec![]);

    assert_eq!(
        shape_of(&effect),
        Shape::Persist(vec![]),
        "persist_all([]) must return Persist([]), not None"
    );
}

#[tokio::test]
async fn tell_constructor_returns_tell_variant() {
    let effect = common::make_tell_effect::<()>().await;

    assert!(
        matches!(effect, SagaEffect::Tell(_)),
        "SagaEffect::tell(proxy, cmd) must return the Tell variant"
    );
}
