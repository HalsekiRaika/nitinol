//! Monoid laws for the post-#45 `SagaEffect` ADT.
//!
//! The variants are `None / Persist { events, tells, schedules } / End /
//! Sequence(Vec<...>)`.  `None` is the identity, `combine` is the associative
//! binary operation that constructs a *flat* `Sequence` (no nesting).
//!
//! These tests do not assert anything about the interpreter — only the
//! algebra of effect composition.

mod common;
use common::{shape_of, Shape};

use nitinol_saga::SagaEffect;

// ---------------------------------------------------------------------------
// Identity laws
// ---------------------------------------------------------------------------

#[test]
fn combine_none_left_is_identity_for_persist() {
    let a = SagaEffect::persist(42u32);
    let result = SagaEffect::None.combine(a);

    assert_eq!(
        shape_of(&result),
        Shape::Persist {
            events: vec![42u32],
            tells: 0,
            schedules: vec![],
        },
        "None.combine(Persist) must return Persist unchanged"
    );
}

#[test]
fn combine_none_right_is_identity_for_persist() {
    let a = SagaEffect::persist(42u32);
    let result = a.combine(SagaEffect::None);

    assert_eq!(
        shape_of(&result),
        Shape::Persist {
            events: vec![42u32],
            tells: 0,
            schedules: vec![],
        },
        "Persist.combine(None) must return Persist unchanged"
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

#[test]
fn combine_none_left_is_identity_for_end() {
    let result: SagaEffect<u32> = SagaEffect::None.combine(SagaEffect::end());

    assert!(
        matches!(result, SagaEffect::End),
        "None.combine(End) must return End unchanged"
    );
}

#[test]
fn combine_none_right_is_identity_for_end() {
    let result: SagaEffect<u32> = SagaEffect::end().combine(SagaEffect::None);

    assert!(
        matches!(result, SagaEffect::End),
        "End.combine(None) must return End unchanged"
    );
}

#[tokio::test]
async fn combine_none_right_identity_holds_for_tell_shaped_persist() {
    let tell = common::make_tell_effect::<u32>().await;
    let result = tell.combine(SagaEffect::None);

    assert_eq!(
        shape_of(&result),
        Shape::Persist {
            events: vec![],
            tells: 1,
            schedules: vec![],
        },
        "Persist-with-tells.combine(None) must return the Persist branch unchanged"
    );
}

// ---------------------------------------------------------------------------
// Associativity
// ---------------------------------------------------------------------------

#[test]
fn combine_is_associative_for_persist_chain() {
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

// ---------------------------------------------------------------------------
// Order preservation & flatness
// ---------------------------------------------------------------------------

#[test]
fn combine_preserves_left_to_right_order() {
    let result = SagaEffect::persist(10u32)
        .combine(SagaEffect::persist(20u32))
        .combine(SagaEffect::persist(30u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![10u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![20u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![30u32],
                tells: 0,
                schedules: vec![],
            },
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
            Shape::Persist {
                events: vec![0u32],
                tells: 0,
                schedules: vec![],
            },
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
            Shape::Persist {
                events: vec![4u32],
                tells: 0,
                schedules: vec![],
            },
        ]),
        "Sequence.combine(Sequence) must extend, not nest"
    );
}

// ---------------------------------------------------------------------------
// Cross-variant composition
// ---------------------------------------------------------------------------

#[test]
fn combine_persist_then_end_yields_ordered_sequence() {
    let result = SagaEffect::persist(7u32).combine(SagaEffect::end());

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![7u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::End,
        ]),
        "Persist.combine(End) must produce Sequence([Persist, End]) preserving order"
    );
}

#[test]
fn combine_end_then_persist_yields_ordered_sequence_even_though_interpreter_will_short_circuit() {
    // The Monoid does not know that End short-circuits subsequent effects at
    // interpretation time — that is the interpreter's responsibility.  The
    // *algebra* must still preserve the left-to-right structure so that the
    // composition `End.combine(Persist)` is observable and testable.
    let result = SagaEffect::end().combine(SagaEffect::persist(9u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::End,
            Shape::Persist {
                events: vec![9u32],
                tells: 0,
                schedules: vec![],
            },
        ]),
        "End.combine(Persist) must produce Sequence([End, Persist]) — \
         the algebra preserves structure, not interpreter semantics"
    );
}

#[tokio::test]
async fn combine_persist_then_tell_shaped_persist_yields_ordered_sequence() {
    let persist = SagaEffect::persist(7u32);
    let tell = common::make_tell_effect::<u32>().await;
    let result = persist.combine(tell);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            Shape::Persist {
                events: vec![7u32],
                tells: 0,
                schedules: vec![],
            },
            Shape::Persist {
                events: vec![],
                tells: 1,
                schedules: vec![],
            },
        ]),
        "Persist.combine(tell()) must produce Sequence with both Persist branches in order. \
         combine never merges two Persist branches even when both are Persist — that would be \
         a semantic change forbidden by spec C-10 (\"現状 MVP の helper.rs ロジックを維持\")."
    );
}
