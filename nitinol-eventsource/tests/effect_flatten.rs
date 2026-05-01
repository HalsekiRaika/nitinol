mod common;
use common::{Shape, shape_of};
use nitinol_eventsource::Effect;

// ---------------------------------------------------------------------------
// flatten()
// ---------------------------------------------------------------------------

/// flatten() of None is a no-op
#[test]
fn flatten_of_none_returns_none() {
    // Given / When
    let result: Effect<u32> = Effect::None.flatten();

    // Then
    assert!(matches!(result, Effect::None), "flatten(None) must return None");
}

/// flatten() of a Persist leaf is a no-op
#[test]
fn flatten_of_persist_leaf_is_noop() {
    // Given
    let effect = Effect::persist(99u32);

    // When
    let result = effect.flatten();

    // Then: unchanged
    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![99u32]),
        "flatten(Persist) must return Persist unchanged"
    );
}

/// flatten() of an Apply leaf is a no-op
#[test]
fn flatten_of_apply_leaf_is_noop() {
    // Given
    let effect = Effect::apply_only(5u32);

    // When
    let result = effect.flatten();

    // Then: unchanged
    assert_eq!(
        shape_of(&result),
        Shape::Apply(vec![5u32]),
        "flatten(Apply) must return Apply unchanged"
    );
}

/// flatten() of a flat Sequence is a no-op
#[test]
fn flatten_of_flat_sequence_is_noop() {
    // Given: Sequence([Persist([1]), Apply([2])]) with no nesting
    let flat = Effect::Sequence(vec![Effect::persist(1u32), Effect::apply_only(2u32)]);

    // When
    let result = flat.flatten();

    // Then: same structure, no change
    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Apply(vec![2u32]),
        ]),
        "flatten of already-flat Sequence must be unchanged"
    );
}

/// flatten() removes one level of Sequence nesting
#[test]
fn flatten_removes_nested_sequence() {
    // Given: Sequence([Sequence([Persist([1]), Persist([2])]), Persist([3])])
    let inner = Effect::Sequence(vec![Effect::persist(1u32), Effect::persist(2u32)]);
    let outer = Effect::Sequence(vec![inner, Effect::persist(3u32)]);

    // When
    let flat = outer.flatten();

    // Then: all three Persist leaves are at the top level
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

/// flatten() recursively removes all levels of nesting (deep nesting becomes fully flat)
#[test]
fn flatten_deeply_nested_sequence_produces_fully_flat_result() {
    // Given: 3-level nested Sequence
    // Sequence([Sequence([Sequence([Persist(1)]), Persist(2)]), Persist(3)])
    let deep = Effect::Sequence(vec![
        Effect::Sequence(vec![
            Effect::Sequence(vec![Effect::persist(1u32)]),
            Effect::persist(2u32),
        ]),
        Effect::persist(3u32),
    ]);

    // When
    let flat = deep.flatten();

    // Then: Sequence([Persist(1), Persist(2), Persist(3)]) — fully flat
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

/// flatten() used after combine produces the same canonical form as direct
/// left-associative combine (because combine already avoids top-level nesting)
#[test]
fn combine_result_and_flatten_match_for_associative_groupings() {
    // Given: two equivalent compose orderings
    let left = Effect::persist(1u32)
        .combine(Effect::apply_only(2u32))
        .combine(Effect::persist(3u32))
        .flatten();

    let right = Effect::persist(1u32)
        .combine(Effect::apply_only(2u32).combine(Effect::persist(3u32)))
        .flatten();

    // Then: after flatten, both groupings are identical
    assert_eq!(
        shape_of(&left),
        shape_of(&right),
        "flattened left- and right-associative combines must be identical"
    );
}
