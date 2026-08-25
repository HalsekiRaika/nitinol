//! Monoid laws for the `SagaEffect` ADT.
//!
//! The variants are `None / Persist { events, tells, schedules } / End /
//! Sequence(Vec<...>) / CancelSchedule`.  `None` is the identity, `combine` is
//! the associative binary operation that constructs a *flat* `Sequence` (no
//! nesting) and folds the newly adjacent `Persist × Persist` junction into a
//! single `Persist` so the pair reaches the store as one atomic batch.
//!
//! These tests do not assert anything about the interpreter — only the
//! algebra of effect composition.  The interpreter-level batch boundary is
//! covered by `saga_combine_persist_single_append.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::{shape_of, Shape};

use std::time::Duration;

use bytes::Bytes;
use nitinol_saga::{SagaEffect, ScheduleSpec, TimerName};

/// Build a distinguishable `ScheduleSpec` so that a reversed concatenation is
/// observable through `Shape` (`ScheduleSpec` is `PartialEq`).
fn spec(name: &str, secs: u64) -> ScheduleSpec {
    ScheduleSpec {
        name: TimerName::new(name),
        after: Duration::from_secs(secs),
        payload: Bytes::new(),
    }
}

fn persist_shape(events: Vec<u32>) -> Shape<u32> {
    Shape::Persist {
        events,
        tells: 0,
        schedules: vec![],
    }
}

#[test]
fn combine_none_left_is_identity_for_persist() {
    let a = SagaEffect::persist(42u32);
    let result = SagaEffect::None.combine(a);

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![42u32]),
        "None.combine(Persist) must return Persist unchanged"
    );
}

#[test]
fn combine_none_right_is_identity_for_persist() {
    let a = SagaEffect::persist(42u32);
    let result = a.combine(SagaEffect::None);

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![42u32]),
        "Persist.combine(None) must return Persist unchanged"
    );
}

#[test]
fn combine_none_with_none_returns_none() {
    let result: SagaEffect<u32> = SagaEffect::None.combine(SagaEffect::None);

    assert!(
        matches!(result, SagaEffect::None),
        "None.combine(None) must return None — the identity must never be \
         promoted to an empty Persist by the Persist-merge path"
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

#[test]
fn combine_none_left_is_identity_for_an_already_merged_persist() {
    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let result = SagaEffect::None.combine(merged);

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![1u32, 2]),
        "None.combine(merged Persist) must return the merged branch unchanged — \
         the identity arm must not re-enter the merge path"
    );
}

#[test]
fn combine_none_right_is_identity_for_an_already_merged_persist() {
    let merged = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));
    let result = merged.combine(SagaEffect::None);

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![1u32, 2]),
        "merged Persist.combine(None) must return the merged branch unchanged"
    );
}

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

    let expected = persist_shape(vec![1u32, 2, 3]);
    assert_eq!(
        shape_of(&left),
        expected,
        "(a.combine(b)).combine(c) must fold the whole Persist chain into one batch"
    );
    assert_eq!(
        shape_of(&right),
        expected,
        "a.combine(b.combine(c)) must fold the whole Persist chain into one batch"
    );
}

#[test]
fn combine_is_associative_when_a_persist_pair_precedes_end() {
    // Right-associated, the pair `P1 ∘ (P2 ∘ End)` combines a leaf with a
    // Sequence — the junction is the Sequence's *head*.  Merging only leaf ×
    // leaf pairs would yield Sequence([P1, P2, End]) here and break the law.
    let left = SagaEffect::persist(1u32)
        .combine(SagaEffect::persist(2u32))
        .combine(SagaEffect::end());
    let right =
        SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32).combine(SagaEffect::end()));

    let expected = Shape::Sequence(vec![persist_shape(vec![1u32, 2]), Shape::End]);
    assert_eq!(
        shape_of(&left),
        expected,
        "(P1.combine(P2)).combine(End) must be Sequence([Persist{{[1,2]}}, End])"
    );
    assert_eq!(
        shape_of(&right),
        expected,
        "P1.combine(P2.combine(End)) must merge across the Sequence head and \
         produce the same value as the left-associated grouping"
    );
}

#[test]
fn combine_is_associative_when_end_precedes_a_persist_pair() {
    // Left-associated, `(End ∘ P1) ∘ P2` combines a Sequence with a leaf — the
    // junction is the Sequence's *tail*.
    let left = SagaEffect::end()
        .combine(SagaEffect::persist(1u32))
        .combine(SagaEffect::persist(2u32));
    let right =
        SagaEffect::end().combine(SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32)));

    let expected = Shape::Sequence(vec![Shape::End, persist_shape(vec![1u32, 2])]);
    assert_eq!(
        shape_of(&left),
        expected,
        "(End.combine(P1)).combine(P2) must merge across the Sequence tail"
    );
    assert_eq!(
        shape_of(&right),
        expected,
        "End.combine(P1.combine(P2)) must produce the same value as the \
         left-associated grouping"
    );
}

#[test]
fn combine_is_associative_when_end_separates_two_persists() {
    let left = SagaEffect::persist(1u32)
        .combine(SagaEffect::end())
        .combine(SagaEffect::persist(2u32));
    let right =
        SagaEffect::persist(1u32).combine(SagaEffect::end().combine(SagaEffect::persist(2u32)));

    // The resulting value is pinned by
    // `combine_does_not_merge_persists_separated_by_end`; this test pins only
    // the law, so a merge that fires for one grouping but not the other fails
    // here even though each grouping looks plausible on its own.
    assert_eq!(
        shape_of(&left),
        shape_of(&right),
        "(P1.combine(End)).combine(P2) and P1.combine(End.combine(P2)) must agree"
    );
}

#[test]
fn combine_is_associative_when_cancel_schedule_separates_two_persists() {
    let mk_cancel = || SagaEffect::<u32>::cancel_schedule(TimerName::new("t"));

    let left = SagaEffect::persist(1u32)
        .combine(mk_cancel())
        .combine(SagaEffect::persist(2u32));
    let right = SagaEffect::persist(1u32).combine(mk_cancel().combine(SagaEffect::persist(2u32)));

    // Value pinned by
    // `combine_does_not_merge_persists_separated_by_cancel_schedule`.
    assert_eq!(
        shape_of(&left),
        shape_of(&right),
        "(P1.combine(Cancel)).combine(P2) and P1.combine(Cancel.combine(P2)) must agree"
    );
}

#[test]
fn combine_merges_two_adjacent_persist_leaves_into_one_persist() {
    let result = SagaEffect::persist(1u32).combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![1u32, 2]),
        "combine must fold two adjacent Persist branches into a single Persist so \
         the interpreter writes one atomic batch"
    );
}

#[tokio::test]
async fn combine_merges_persist_with_tell_shaped_persist() {
    let persist = SagaEffect::persist(7u32);
    let tell = common::make_tell_effect::<u32>().await;
    let result = persist.combine(tell);

    assert_eq!(
        shape_of(&result),
        Shape::Persist {
            events: vec![7u32],
            tells: 1,
            schedules: vec![],
        },
        "persist(e).combine(tell(..)) must yield ONE Persist carrying both the \
         domain event and the tell intent — splitting them across two Persist \
         branches reopens the at-most-once tell-loss window"
    );
}

#[tokio::test]
async fn combine_concatenates_events_tells_and_schedules_left_to_right() {
    let left_spec = spec("left", 1);
    let right_spec = spec("right", 2);
    let left_intent = common::make_tell_intent().await;
    let right_intent = common::make_tell_intent().await;

    let left = SagaEffect::persist_all(vec![1u32, 2])
        .combine(SagaEffect::tell_intent(left_intent))
        .combine(SagaEffect::schedule_spec(left_spec.clone()));
    let right = SagaEffect::persist(3u32)
        .combine(SagaEffect::tell_intent(right_intent))
        .combine(SagaEffect::schedule_spec(right_spec.clone()));

    assert_eq!(
        shape_of(&left.combine(right)),
        Shape::Persist {
            events: vec![1u32, 2, 3],
            tells: 2,
            schedules: vec![left_spec, right_spec],
        },
        "the merge must concatenate events, tells AND schedules — each in \
         left-to-right order; dropping or reordering any of the three categories \
         silently loses saga intent"
    );
}

#[test]
fn combine_preserves_left_to_right_order() {
    let result = SagaEffect::persist(10u32)
        .combine(SagaEffect::persist(20u32))
        .combine(SagaEffect::persist(30u32));

    assert_eq!(
        shape_of(&result),
        persist_shape(vec![10u32, 20, 30]),
        "combine must preserve left-to-right order when folding events"
    );
}

#[test]
fn combine_does_not_merge_persists_separated_by_end() {
    let result = SagaEffect::persist(1u32)
        .combine(SagaEffect::end())
        .combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::End,
            persist_shape(vec![2u32]),
        ]),
        "only the *adjacent* junction merges; scanning back past End for the last \
         Persist would smuggle a post-End effect into the pre-End batch and break \
         the End short-circuit asserted end-to-end by saga_end_variant.rs"
    );
}

#[test]
fn combine_does_not_merge_persists_separated_by_cancel_schedule() {
    let result = SagaEffect::persist(1u32)
        .combine(SagaEffect::cancel_schedule(TimerName::new("drop")))
        .combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::CancelSchedule,
            persist_shape(vec![2u32]),
        ]),
        "CancelSchedule is a leaf of its own — it must neither be absorbed into a \
         Persist nor be skipped over when looking for the merge junction"
    );
}

#[test]
fn combine_merges_persists_across_a_trailing_none_leaf() {
    // `combine` itself never builds a Sequence holding `None` — the identity arm
    // short-circuits first — but the variant is public, so a hand-built Sequence
    // can put one on the junction.
    let seq = SagaEffect::Sequence(vec![SagaEffect::persist(1u32), SagaEffect::None]);
    let result = seq.combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32, 2])]),
        "None is the identity: it must not sit on the junction and split the two \
         Persist branches into separate append batches — an identity element that \
         decides whether a tell survives a crash is observable, which the identity \
         law forbids.  Unlike End and CancelSchedule, None carries no \
         interpretation of its own"
    );
}

#[test]
fn combine_merges_persists_across_a_leading_none_leaf() {
    // Mirror of the trailing case: here the hand-built Sequence is on the right
    // of the junction, so its *head* is the None to look past.
    let seq = SagaEffect::Sequence(vec![SagaEffect::None, SagaEffect::persist(2u32)]);
    let result = SagaEffect::persist(1u32).combine(seq);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32, 2])]),
        "the identity must be dropped on the head junction exactly as on the tail \
         junction; merging on one side only would make the batch boundary depend \
         on which side the hand-built Sequence was passed"
    );
}

#[test]
fn combine_does_not_merge_across_end_hidden_behind_a_none_leaf() {
    // Dropping the identity must expose the *real* junction element, not skip
    // past it: the End behind a hand-built trailing None is still End.
    let seq = SagaEffect::Sequence(vec![
        SagaEffect::persist(1u32),
        SagaEffect::end(),
        SagaEffect::None,
    ]);
    let result = seq.combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::End,
            persist_shape(vec![2u32]),
        ]),
        "discarding the identity must not let the merge reach across the End \
         behind it and smuggle a post-End effect into the pre-End batch"
    );
}

#[test]
fn combining_sequence_with_new_effect_appends_not_nests() {
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let extra = SagaEffect::persist(2u32);
    let result = seq.combine(extra);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::End,
            persist_shape(vec![2u32]),
        ]),
        "Sequence.combine(leaf) must append, not nest"
    );
}

#[test]
fn combining_sequence_whose_tail_is_persist_merges_at_the_junction() {
    let seq = SagaEffect::end().combine(SagaEffect::persist(1u32));
    let result = seq.combine(SagaEffect::persist(2u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![Shape::End, persist_shape(vec![1u32, 2])]),
        "Sequence.combine(Persist) must fold the trailing Persist of the Sequence \
         with the incoming one"
    );
}

#[test]
fn combining_new_effect_with_sequence_prepends_not_nests() {
    let head = SagaEffect::persist(0u32);
    let seq = SagaEffect::end().combine(SagaEffect::persist(1u32));
    let result = head.combine(seq);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![0u32]),
            Shape::End,
            persist_shape(vec![1u32]),
        ]),
        "leaf.combine(Sequence) must prepend, not nest"
    );
}

#[test]
fn combining_persist_with_sequence_whose_head_is_persist_merges_at_the_junction() {
    let seq = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let result = SagaEffect::persist(0u32).combine(seq);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![0u32, 1]), Shape::End]),
        "Persist.combine(Sequence) must fold the incoming Persist with the leading \
         Persist of the Sequence"
    );
}

#[test]
fn combining_two_sequences_concatenates_them_flat() {
    let left = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let right = SagaEffect::persist(2u32).combine(SagaEffect::end());
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::End,
            persist_shape(vec![2u32]),
            Shape::End,
        ]),
        "Sequence.combine(Sequence) must extend, not nest — and must not merge \
         across the End sitting on the junction"
    );
}

#[test]
fn combining_two_sequences_merges_only_the_junction_pair() {
    let left = SagaEffect::end().combine(SagaEffect::persist(1u32));
    let right = SagaEffect::persist(2u32).combine(SagaEffect::end());
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![Shape::End, persist_shape(vec![1u32, 2]), Shape::End,]),
        "Sequence.combine(Sequence) must fold exactly the tail × head junction \
         and leave every other leaf untouched"
    );
}

#[test]
fn combining_two_sequences_merges_across_a_leading_none_in_the_right_sequence() {
    // Both operands are Sequences, so the junction is the left tail against the
    // right *head* — and that head is a hand-built identity.
    let left = SagaEffect::Sequence(vec![SagaEffect::persist(1u32)]);
    let right = SagaEffect::Sequence(vec![SagaEffect::None, SagaEffect::persist(2u32)]);
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32, 2])]),
        "Sequence.combine(Sequence) must look past the identity leading the right \
         sequence exactly as leaf.combine(Sequence) does; leaving it on the \
         junction makes the batch boundary depend on whether the right operand \
         happens to be a Sequence"
    );
}

#[test]
fn combining_two_sequences_merges_across_identities_on_both_sides_of_the_junction() {
    let left = SagaEffect::Sequence(vec![SagaEffect::persist(1u32), SagaEffect::None]);
    let right = SagaEffect::Sequence(vec![SagaEffect::None, SagaEffect::persist(2u32)]);
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32, 2])]),
        "an identity on either side of the junction — or on both — must be \
         dropped before the two Persist branches are folded"
    );
}

#[test]
fn combining_two_sequences_does_not_merge_across_end_hidden_behind_a_leading_none() {
    // Dropping the identity must expose the *real* junction element, not skip
    // past it: the End behind the right sequence's leading None is still End.
    let left = SagaEffect::Sequence(vec![SagaEffect::persist(1u32)]);
    let right = SagaEffect::Sequence(vec![
        SagaEffect::None,
        SagaEffect::end(),
        SagaEffect::persist(2u32),
    ]);
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![
            persist_shape(vec![1u32]),
            Shape::End,
            persist_shape(vec![2u32]),
        ]),
        "discarding the identity must not let the merge reach across the End \
         behind it and smuggle a post-End effect into the pre-End batch"
    );
}

#[test]
fn combining_a_sequence_with_an_identity_only_sequence_keeps_the_left_untouched() {
    let left = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let right = SagaEffect::Sequence(vec![SagaEffect::None, SagaEffect::None]);
    let result = left.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32]), Shape::End]),
        "a right sequence made only of identities contributes no junction \
         element — every leaf of it is dropped and the left sequence stands"
    );
}

#[test]
fn combine_is_associative_when_a_none_leads_the_right_sequence() {
    // The identity sits at the head of the middle Sequence, so the *left*
    // grouping meets it as `Sequence × Sequence` while the right grouping first
    // folds it away.  Pinning the merged value (not just the two groupings'
    // equality) fails a `combine` that merely bypasses the identity
    // consistently in both groupings without folding the exposed pair.
    let mk = || {
        (
            SagaEffect::Sequence(vec![SagaEffect::persist(1u32)]),
            SagaEffect::Sequence(vec![SagaEffect::None, SagaEffect::persist(2u32)]),
            SagaEffect::Sequence(vec![SagaEffect::persist(3u32)]),
        )
    };

    let (a, b, c) = mk();
    let left = a.combine(b).combine(c);

    let (a, b, c) = mk();
    let right = a.combine(b.combine(c));

    let expected = Shape::Sequence(vec![persist_shape(vec![1u32, 2, 3])]);
    assert_eq!(
        shape_of(&left),
        expected,
        "(a.combine(b)).combine(c) must fold the whole Persist chain across the \
         identity into one batch"
    );
    assert_eq!(
        shape_of(&right),
        expected,
        "a.combine(b.combine(c)) must produce the same value as the \
         left-associated grouping"
    );
}

#[test]
fn combining_empty_hand_built_sequence_with_persist_keeps_the_leaf() {
    let empty: SagaEffect<u32> = SagaEffect::Sequence(vec![]);
    let result = empty.combine(SagaEffect::persist(1u32));

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32])]),
        "an empty hand-built Sequence has no junction to merge against; the \
         incoming leaf must simply be appended"
    );
}

#[test]
fn combining_persist_with_empty_hand_built_sequence_keeps_the_leaf() {
    let empty: SagaEffect<u32> = SagaEffect::Sequence(vec![]);
    let result = SagaEffect::persist(1u32).combine(empty);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32])]),
        "combining with an empty hand-built Sequence must keep the leaf and must \
         not look for a junction element that does not exist"
    );
}

#[test]
fn combining_sequence_with_an_empty_hand_built_sequence_keeps_every_leaf() {
    let left = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let empty: SagaEffect<u32> = SagaEffect::Sequence(vec![]);
    let result = left.combine(empty);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32]), Shape::End]),
        "an empty right-hand Sequence contributes no junction element; taking its \
         head unconditionally would panic instead of leaving the left Sequence intact"
    );
}

#[test]
fn combining_an_empty_hand_built_sequence_with_a_sequence_keeps_every_leaf() {
    let empty: SagaEffect<u32> = SagaEffect::Sequence(vec![]);
    let right = SagaEffect::persist(1u32).combine(SagaEffect::end());
    let result = empty.combine(right);

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![1u32]), Shape::End]),
        "an empty left-hand Sequence has no tail to merge against; the incoming \
         leaves must simply be appended in order"
    );
}

#[test]
fn combine_persist_then_end_yields_ordered_sequence() {
    let result = SagaEffect::persist(7u32).combine(SagaEffect::end());

    assert_eq!(
        shape_of(&result),
        Shape::Sequence(vec![persist_shape(vec![7u32]), Shape::End]),
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
        Shape::Sequence(vec![Shape::End, persist_shape(vec![9u32])]),
        "End.combine(Persist) must produce Sequence([End, Persist]) — \
         the algebra preserves structure, not interpreter semantics"
    );
}
