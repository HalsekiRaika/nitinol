mod common;
use common::{shape_of, Shape};

use nitinol_saga::SagaEffect;

#[test]
fn combine_none_left_is_identity() {
    let a = SagaEffect::persist(42u32);
    let result = SagaEffect::None.combine(a);

    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![42u32]),
        "None.combine(a) must return a unchanged"
    );
}

#[test]
fn combine_none_right_is_identity() {
    let a = SagaEffect::persist(42u32);
    let result = a.combine(SagaEffect::None);

    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![42u32]),
        "a.combine(None) must return a unchanged"
    );
}

#[test]
fn combine_none_with_none_returns_none() {
    let result: SagaEffect<u32> = SagaEffect::None.combine(SagaEffect::None);

    assert!(
        matches!(result, SagaEffect::None),
        "None.combine(None) must return None"
    );
}

#[tokio::test]
async fn combine_none_right_identity_holds_for_tell_variant() {
    let tell = common::make_tell_effect::<()>().await;
    let result = tell.combine(SagaEffect::None);

    assert!(
        matches!(result, SagaEffect::Tell(_)),
        "Tell.combine(None) must return Tell unchanged"
    );
}

#[test]
fn combine_is_associative() {
    let mk = || {
        (
            SagaEffect::persist(1u32),
            SagaEffect::persist(2u32),
            SagaEffect::persist(3u32),
        )
    };

    let (a, b, c) = mk();
    let left = a.combine(b).combine(c);

    let (a, b, c) = mk();
    let right = a.combine(b.combine(c));

    let expected = Shape::Sequence(vec![
        Shape::Persist(vec![1u32]),
        Shape::Persist(vec![2u32]),
        Shape::Persist(vec![3u32]),
    ]);
    assert_eq!(
        shape_of(&left),
        expected,
        "(a.combine(b)).combine(c) must equal Sequence([a, b, c])"
    );
    assert_eq!(
        shape_of(&right),
        expected,
        "a.combine(b.combine(c)) must equal Sequence([a, b, c])"
    );
}

#[test]
fn combine_preserves_left_to_right_order() {
    let a = SagaEffect::persist(10u32);
    let b = SagaEffect::persist(20u32);
    let c = SagaEffect::persist(30u32);
    let result = a.combine(b).combine(c);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![10u32]),
            Shape::Persist(vec![20u32]),
            Shape::Persist(vec![30u32]),
        ]),
        "combine must preserve left-to-right order"
    );
}

#[test]
fn combining_sequence_with_new_effect_appends_not_nests() {
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let extra = SagaEffect::persist(3u32);
    let result = seq.combine(extra);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Persist(vec![2u32]),
            Shape::Persist(vec![3u32]),
        ]),
        "Sequence.combine(leaf) must append, not nest"
    );
}

#[test]
fn combining_new_effect_with_sequence_prepends_not_nests() {
    let head = SagaEffect::persist(0u32);
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let result = head.combine(seq);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![0u32]),
            Shape::Persist(vec![1u32]),
            Shape::Persist(vec![2u32]),
        ]),
        "leaf.combine(Sequence) must prepend, not nest"
    );
}

#[test]
fn combining_two_sequences_concatenates_them_flat() {
    let left = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let right = SagaEffect::persist(3u32).combine(SagaEffect::persist(4u32));
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Persist(vec![2u32]),
            Shape::Persist(vec![3u32]),
            Shape::Persist(vec![4u32]),
        ]),
        "Sequence.combine(Sequence) must extend, not nest"
    );
}

#[tokio::test]
async fn combine_persist_then_tell_yields_ordered_sequence() {
    let persist = SagaEffect::persist(7u32);
    let tell = common::make_tell_effect::<u32>().await;
    let result = persist.combine(tell);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![Shape::Persist(vec![7u32]), Shape::Tell]),
        "Persist.combine(Tell) must produce Sequence([Persist, Tell]) in order"
    );
}
