//! `flatten` tests for the post-#45 `SagaEffect` ADT.
//!
//! `flatten` recursively hoists inner `Sequence` elements to the parent level,
//! collapses all-None to None, and unwraps single-element sequences.  Leaf
//! variants (Persist, End) are returned unchanged.

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
fn flatten_of_end_returns_end() {
    let result: SagaEffect<u32> = SagaEffect::end().flatten();

    assert!(
        matches!(result, SagaEffect::End),
        "flatten(End) must return End unchanged — End is a leaf variant"
    );
}

#[test]
fn flatten_of_persist_leaf_is_noop() {
    let effect = SagaEffect::persist(99u32);
    let result = effect.flatten();

    assert_eq!(
        shape_of(&result),
        Shape::Persist {
            events: vec![99u32],
            tells: 0,
            schedules: vec![],
        },
        "flatten(Persist) must return Persist unchanged"
    );
}

#[test]
fn flatten_of_flat_sequence_is_noop() {
    let flat = SagaEffect::Sequence(vec![SagaEffect::persist(1u32), SagaEffect::persist(2u32)]);
    let result = flat.flatten();

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![1u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![2u32],
                tells: 0,
                schedules: vec![],
            },
        ]),
        "flatten of a flat Sequence must be unchanged"
    );
}

#[test]
fn flatten_removes_nested_sequence() {
    let inner = SagaEffect::Sequence(vec![SagaEffect::persist(1u32), SagaEffect::persist(2u32)]);
    let outer = SagaEffect::Sequence(vec![inner, SagaEffect::persist(3u32)]);
    let flat = outer.flatten();

    assert_eq!(
        shape_of(&flat),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![1u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![2u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![3u32],
                tells: 0,
                schedules: vec![],
            },
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
        _ => panic!("expected Sequence after flattening deeply nested Sequence, got: {shape:?}"),
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
fn flatten_single_element_sequence_unwraps_to_child_persist() {
    let seq = SagaEffect::Sequence(vec![SagaEffect::persist(123u32)]);
    let flat = seq.flatten();

    assert_eq!(
        shape_of(&flat),
        Shape::Persist {
            events: vec![123u32],
            tells: 0,
            schedules: vec![],
        },
        "single-element Sequence must unwrap to its inner leaf"
    );
}

#[test]
fn flatten_single_element_sequence_unwraps_to_child_end() {
    let seq: SagaEffect<u32> = SagaEffect::Sequence(vec![SagaEffect::end()]);
    let flat = seq.flatten();

    assert!(
        matches!(flat, SagaEffect::End),
        "single-element Sequence([End]) must unwrap to End"
    );
}

#[test]
fn flatten_sequence_with_end_keeps_end_inline() {
    let seq = SagaEffect::Sequence(vec![
        SagaEffect::persist(1u32),
        SagaEffect::end(),
        SagaEffect::persist(2u32),
    ]);
    let flat = seq.flatten();

    assert_eq!(
        shape_of(&flat),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![1u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::End,
            Shape::Persist {
                events: vec![2u32],
                tells: 0,
                schedules: vec![],
            },
        ]),
        "End in the middle of a Sequence must remain at its original position after flatten — \
         flatten is structural, not interpretive"
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
