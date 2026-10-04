//! What the deadline effects still look like from outside the crate.
//!
//! A deadline is set and cancelled through the `Deadline<M>` component the
//! producer is handed, so neither `SagaEffect::schedule` nor a `ScheduleSpec`
//! literal exists any more: the only way to name a timer is to ask for the
//! component, which is what leaves effect → append → arm as the one expressible
//! path. The construction surface those builders had is therefore pinned where
//! the values can be built:
//!
//! - `Deadline::set` / `Deadline::cancel`'s shape, and the message being written
//!   through the codec its own type names — `src/components/deadline.rs`.
//! - the schedule-carrying cases of the effect algebra — `src/effect/helper.rs`.
//! - the whole path from setting a deadline to its firing reaching `handle` —
//!   `saga_deadline_scheduler.rs`.
//!
//! What stays observable from out here is the cancellation leaf itself:
//! `SagaEffect::CancelSchedule(TimerName)` is a public variant, and it is a leaf
//! of its own — never absorbed into a `Persist`, never swallowed like the
//! identity.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_schedule_effect`.

use nitinol_saga::{SagaEffect, TimerName};

#[test]
fn cancel_combines_after_persist_preserving_order() {
    // Given a decision and a cancellation
    let persist = SagaEffect::persist(1u32);
    let cancel = SagaEffect::<u32>::CancelSchedule(TimerName::new("drop"));

    // When combined via the Monoid operation
    let combined = persist.combine(cancel);

    // Then order is preserved as a flat Sequence [Persist, CancelSchedule]
    match combined {
        SagaEffect::Sequence(children) => {
            assert_eq!(children.len(), 2, "combine must not nest or drop leaves");
            assert!(matches!(children[0], SagaEffect::Persist { .. }));
            assert!(matches!(children[1], SagaEffect::CancelSchedule(_)));
        }
        _ => panic!("combining Persist with CancelSchedule must yield a Sequence"),
    }
}

#[test]
fn cancel_survives_flatten_unchanged() {
    // Given a nested sequence around a CancelSchedule
    let nested = SagaEffect::<u32>::Sequence(vec![
        SagaEffect::None,
        SagaEffect::Sequence(vec![SagaEffect::CancelSchedule(TimerName::new("x"))]),
    ]);

    // When flattened
    let flat = nested.flatten();

    // Then the lone CancelSchedule is unwrapped intact (not swallowed like None)
    match flat {
        SagaEffect::CancelSchedule(name) => assert_eq!(name, TimerName::new("x")),
        _ => panic!("flatten must unwrap the single CancelSchedule leaf unchanged"),
    }
}
