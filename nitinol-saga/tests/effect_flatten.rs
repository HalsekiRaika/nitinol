mod common;
use common::{shape_of, Shape};
use nitinol_saga::SagaEffect;

#[test]
fn flatten_of_none_returns_none() {
    let result: SagaEffect<u32> = SagaEffect::None.flatten();

    assert!(
        matches!(result, SagaEffect::None),
        "flatten(None) must return None"
    );
}

#[test]
fn flatten_of_persist_leaf_is_noop() {
    let effect = SagaEffect::persist(99u32);
    let result = effect.flatten();

    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![99u32]),
        "flatten(Persist) must return Persist unchanged"
    );
}

#[test]
fn flatten_of_flat_sequence_is_noop() {
    let flat = SagaEffect::Sequence(vec![
        SagaEffect::persist(1u32),
        SagaEffect::persist(2u32),
    ]);
    let result = flat.flatten();

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Persist(vec![2u32]),
        ]),
        "flatten of a flat Sequence must be unchanged"
    );
}

#[test]
fn flatten_removes_nested_sequence() {
    let inner = SagaEffect::Sequence(vec![
        SagaEffect::persist(1u32),
        SagaEffect::persist(2u32),
    ]);
    let outer = SagaEffect::Sequence(vec![inner, SagaEffect::persist(3u32)]);
    let flat = outer.flatten();

    assert_eq!(
        shape_of(&flat),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Persist(vec![2u32]),
            Shape::Persist(vec![3u32]),
        ]),
        "flatten() must hoist inner Sequence elements to the parent level"
    );
}

#[test]
fn flatten_deeply_nested_sequence_produces_fully_flat_result() {
    let deep = SagaEffect::Sequence(vec![
        SagaEffect::Sequence(vec![
            SagaEffect::Sequence(vec![SagaEffect::persist(1u32)]),
            SagaEffect::persist(2u32),
        ]),
        SagaEffect::persist(3u32),
    ]);
    let flat = deep.flatten();

    let shape = shape_of(&flat);
    match &shape {
        Shape::Sequence(children) => {
            assert_eq!(
                children.len(),
                3,
                "3-element deep nesting must flatten to 3 top-level elements"
            );
            for child in children {
                assert!(
                    !matches!(child, Shape::Sequence(_)),
                    "no nested Sequence must remain after flatten"
                );
            }
        }
        _ => panic!(
            "expected Sequence after flattening deeply nested Sequence, got: {shape:?}"
        ),
    }
}

#[test]
fn flatten_sequence_of_only_none_collapses_to_none() {
    let seq: SagaEffect<u32> = SagaEffect::Sequence(vec![
        SagaEffect::None,
        SagaEffect::Sequence(vec![SagaEffect::None]),
    ]);
    let flat = seq.flatten();

    assert!(
        matches!(flat, SagaEffect::None),
        "Sequence containing only None must collapse to None"
    );
}

#[test]
fn flatten_single_element_sequence_unwraps_to_child() {
    let seq = SagaEffect::Sequence(vec![SagaEffect::persist(123u32)]);
    let flat = seq.flatten();

    assert_eq!(
        shape_of(&flat),
        Shape::Persist(vec![123u32]),
        "single-element Sequence must unwrap to its inner leaf"
    );
}

#[test]
fn combine_result_and_flatten_match_for_associative_groupings() {
    let left = SagaEffect::persist(1u32)
        .combine(SagaEffect::persist(2u32))
        .combine(SagaEffect::persist(3u32))
        .flatten();

    let right = SagaEffect::persist(1u32)
        .combine(SagaEffect::persist(2u32).combine(SagaEffect::persist(3u32)))
        .flatten();

    assert_eq!(
        shape_of(&left),
        shape_of(&right),
        "flattened left- and right-associative combines must be identical"
    );
}
