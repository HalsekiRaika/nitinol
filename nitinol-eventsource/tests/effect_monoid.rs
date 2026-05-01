mod common;
use common::{Shape, TestMsg, TestProcess, shape_of};
use nitinol_eventsource::Effect;
use nitinol_runtime::process::Props;
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Monoid: identity element (None)
// ---------------------------------------------------------------------------

/// None combined on the left is the identity: None.combine(a) == a
#[test]
fn combine_none_left_is_identity() {
    // Given
    let a = Effect::persist(42u32);

    // When
    let result = Effect::None.combine(a);

    // Then: must be Persist([42]), not wrapped in Sequence
    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![42u32]),
        "None.combine(a) must return a unchanged"
    );
}

/// None combined on the right is the identity: a.combine(None) == a
#[test]
fn combine_none_right_is_identity() {
    // Given
    let a = Effect::persist(42u32);

    // When
    let result = a.combine(Effect::None);

    // Then: must be Persist([42]), not wrapped in Sequence
    assert_eq!(
        shape_of(&result),
        Shape::Persist(vec![42u32]),
        "a.combine(None) must return a unchanged"
    );
}

/// None.combine(None) returns None (double identity preserves unit element)
#[test]
fn combine_none_with_none_returns_none() {
    // Given / When
    let result: Effect<u32> = Effect::None.combine(Effect::None);

    // Then
    assert!(
        matches!(result, Effect::None),
        "None.combine(None) must return None"
    );
}

/// Side effect combined with None on the right is the identity
#[tokio::test]
async fn combine_none_right_identity_holds_for_side_variant() {
    // Given: a tell effect (Side variant)
    let system = ProcessSystem::new().await;
    let proxy = system.spawn(Props::new(|| TestProcess)).await;
    let tell = Effect::<()>::tell(proxy, TestMsg);

    // When
    let result = tell.combine(Effect::None);

    // Then: Side variant is preserved
    assert!(
        matches!(result, Effect::Side(_)),
        "Side.combine(None) must return Side unchanged"
    );
}

// ---------------------------------------------------------------------------
// Monoid: associativity
// ---------------------------------------------------------------------------

/// (a.combine(b)).combine(c) and a.combine(b.combine(c)) produce identical shapes
#[test]
fn combine_is_associative() {
    // Given: three distinct non-None effects
    let mk = || {
        (
            Effect::persist(1u32),
            Effect::apply_only(2u32),
            Effect::persist(3u32),
        )
    };

    // When: left-associative grouping: (a + b) + c
    let (a, b, c) = mk();
    let left = a.combine(b).combine(c);

    // When: right-associative grouping: a + (b + c)
    let (a, b, c) = mk();
    let right = a.combine(b.combine(c));

    // Then: both produce the same canonical Sequence([Persist([1]), Apply([2]), Persist([3])])
    let expected = Shape::Sequence(vec![
        Shape::Persist(vec![1u32]),
        Shape::Apply(vec![2u32]),
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

/// combine preserves left-to-right element order
#[test]
fn combine_preserves_left_to_right_order() {
    // Given: three Persist effects with distinct values
    let a = Effect::persist(10u32);
    let b = Effect::persist(20u32);
    let c = Effect::persist(30u32);

    // When
    let result = a.combine(b).combine(c);

    // Then: elements appear in the original order
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

/// Combining a Sequence with another effect appends rather than nesting
#[test]
fn combining_sequence_with_new_effect_appends_not_nests() {
    // Given: an existing Sequence([Persist(1), Apply(2)]) and a new Persist(3)
    let seq = Effect::persist(1u32).combine(Effect::apply_only(2u32));
    let extra = Effect::persist(3u32);

    // When
    let result = seq.combine(extra);

    // Then: flat Sequence of three elements, no inner Sequence
    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Apply(vec![2u32]),
            Shape::Persist(vec![3u32]),
        ]),
        "combining Sequence + leaf must append, not nest"
    );
}

/// Prepending to a Sequence via combine inserts at the front
#[test]
fn combining_new_effect_with_sequence_prepends_not_nests() {
    // Given: a new Persist(0) and an existing Sequence([Persist(1), Apply(2)])
    let head = Effect::persist(0u32);
    let seq = Effect::persist(1u32).combine(Effect::apply_only(2u32));

    // When
    let result = head.combine(seq);

    // Then: flat Sequence starting with Persist(0)
    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![0u32]),
            Shape::Persist(vec![1u32]),
            Shape::Apply(vec![2u32]),
        ]),
        "combining leaf + Sequence must prepend the leaf, not nest"
    );
}

/// Combining two Sequences merges them into a single flat Sequence without nesting.
///
/// Exercises the `(Sequence, Sequence) => extend` arm of `combine`, which is the
/// critical path when parallel sub-sequences are assembled (e.g. `(a+b).combine(c+d)`).
#[test]
fn combining_two_sequences_concatenates_them_flat() {
    // Given: two existing Sequences
    let left = Effect::persist(1u32).combine(Effect::apply_only(2u32));
    let right = Effect::persist(3u32).combine(Effect::apply_only(4u32));

    // When
    let result = left.combine(right);

    // Then: a single flat Sequence with all four leaves, no nesting
    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist(vec![1u32]),
            Shape::Apply(vec![2u32]),
            Shape::Persist(vec![3u32]),
            Shape::Apply(vec![4u32]),
        ]),
        "Sequence.combine(Sequence) must extend, not nest"
    );
}
