//! Construction / Builder tests for the DeadlineScheduler effect surface.
//!
//! These pin the public effect API introduced by the scheduler:
//! - `ScheduleSpec { name, after, payload }` replaces the old `Schedule { at }`.
//! - `SagaEffect::Persist { schedules }` now carries `Vec<ScheduleSpec>`.
//! - `SagaEffect::with_schedules(Vec<ScheduleSpec>)` attaches schedules to a
//!   `Persist` branch (set, not merge — same semantics as `with_tells`).
//! - `SagaEffect::schedule(name, after, msg)` is the typed convenience builder
//!   that serialises the scheduled message into the `ScheduleSpec`'s `payload`.
//! - `SagaEffect::CancelSchedule(TimerName)` is the new name-scoped cancel
//!   variant, reachable via `SagaEffect::cancel_schedule(name)`.
//!
//! The tests are intentionally self-contained (no `mod common`) so they do not
//! couple to the shared `Shape` helper, which still mirrors the older
//! `Schedule { at }` layout and is migrated separately.

use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_saga::{SagaEffect, ScheduleSpec, TimerName};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reminder {
    note: String,
}

/// Borrow the single `Persist` branch of an effect, failing loudly otherwise.
fn persist_schedules<E>(effect: &SagaEffect<E>) -> &[ScheduleSpec] {
    match effect {
        SagaEffect::Persist { schedules, .. } => schedules,
        _ => panic!("expected a Persist branch carrying schedules"),
    }
}

#[test]
fn schedule_spec_holds_name_after_and_payload_verbatim() {
    // Given a name, a relative delay and an opaque payload
    let name = TimerName::new("reminder");
    let after = Duration::from_secs(30);
    let payload = Bytes::from_static(b"opaque-bytes");

    // When a ScheduleSpec is constructed
    let spec = ScheduleSpec {
        name: name.clone(),
        after,
        payload: payload.clone(),
    };

    // Then each field is retained exactly as supplied
    assert_eq!(spec.name, name, "ScheduleSpec must retain its TimerName");
    assert_eq!(
        spec.after, after,
        "ScheduleSpec must retain the relative `after` duration (not an absolute instant)"
    );
    assert_eq!(
        spec.payload, payload,
        "ScheduleSpec must retain the payload bytes verbatim"
    );
}

#[test]
fn with_schedules_attaches_specs_to_persist_branch_in_order() {
    // Given two schedule specs
    let first = ScheduleSpec {
        name: TimerName::new("a"),
        after: Duration::from_millis(100),
        payload: Bytes::from_static(b"1"),
    };
    let second = ScheduleSpec {
        name: TimerName::new("b"),
        after: Duration::from_millis(200),
        payload: Bytes::from_static(b"2"),
    };

    // When they are attached to a Persist branch
    let effect = SagaEffect::persist(7u32).with_schedules(vec![first.clone(), second.clone()]);

    // Then the branch keeps both, in order, alongside the user event
    let schedules = persist_schedules(&effect);
    assert_eq!(schedules.len(), 2, "both schedules must be attached");
    assert_eq!(schedules[0].name, first.name);
    assert_eq!(schedules[1].name, second.name);
    match &effect {
        SagaEffect::Persist { events, tells, .. } => {
            assert_eq!(events, &vec![7u32], "the user event must be preserved");
            assert!(tells.is_empty(), "no tells must be introduced");
        }
        _ => unreachable!("persist_schedules already asserted the Persist branch"),
    }
}

#[test]
fn with_schedules_is_set_not_merge() {
    // Given a Persist branch that already carries one schedule
    let old = ScheduleSpec {
        name: TimerName::new("old"),
        after: Duration::from_secs(1),
        payload: Bytes::new(),
    };
    let new = ScheduleSpec {
        name: TimerName::new("new"),
        after: Duration::from_secs(2),
        payload: Bytes::new(),
    };

    // When with_schedules is called twice
    let effect = SagaEffect::persist(1u32)
        .with_schedules(vec![old])
        .with_schedules(vec![new.clone()]);

    // Then only the final list survives (set, not merge)
    let schedules = persist_schedules(&effect);
    assert_eq!(schedules.len(), 1, "the second call must replace the first");
    assert_eq!(schedules[0].name, new.name);
}

#[test]
fn schedule_builder_serialises_message_into_payload() {
    // Given a typed scheduled message
    let msg = Reminder {
        note: "ping".to_owned(),
    };
    let after = Duration::from_secs(5);

    // When the typed convenience builder is used
    let effect = SagaEffect::<u32>::schedule(TimerName::new("reminder"), after, msg.clone());

    // Then it yields a Persist branch with exactly one schedule entry whose payload is the
    // serialised message, carrying the given name and delay
    let schedules = persist_schedules(&effect);
    assert_eq!(
        schedules.len(),
        1,
        "schedule() must produce exactly one schedule entry"
    );
    assert_eq!(schedules[0].name, TimerName::new("reminder"));
    assert_eq!(schedules[0].after, after);

    let decoded: Reminder =
        serde_json::from_slice(&schedules[0].payload).expect("payload must decode to the message");
    assert_eq!(
        decoded, msg,
        "schedule() must serialise the message round-trippably into the payload"
    );
}

#[test]
fn schedule_builder_produces_no_user_events_or_tells() {
    // Given a scheduled message
    let effect = SagaEffect::<u32>::schedule(
        TimerName::new("reminder"),
        Duration::from_secs(1),
        Reminder {
            note: "x".to_owned(),
        },
    );

    // Then the branch carries only the schedule — no events, no tells
    match &effect {
        SagaEffect::Persist {
            events,
            tells,
            schedules,
        } => {
            assert!(
                events.is_empty(),
                "schedule() must not persist a user event"
            );
            assert!(tells.is_empty(), "schedule() must not attach a tell");
            assert_eq!(schedules.len(), 1);
        }
        _ => panic!("schedule() must return a Persist branch"),
    }
}

#[test]
fn cancel_schedule_builder_returns_cancel_variant_with_name() {
    // Given a timer name
    let name = TimerName::new("reminder");

    // When cancel_schedule is built
    let effect = SagaEffect::<u32>::cancel_schedule(name.clone());

    // Then it is the CancelSchedule variant carrying that name
    match effect {
        SagaEffect::CancelSchedule(carried) => assert_eq!(
            carried, name,
            "cancel_schedule(name) must carry the requested TimerName"
        ),
        _ => panic!("cancel_schedule must return SagaEffect::CancelSchedule"),
    }
}

#[test]
fn cancel_schedule_variant_is_constructible_directly() {
    // The variant is public and must be matchable/constructible.
    let effect: SagaEffect<u32> = SagaEffect::CancelSchedule(TimerName::new("t"));
    assert!(matches!(effect, SagaEffect::CancelSchedule(_)));
}

#[test]
fn cancel_schedule_combines_after_persist_preserving_order() {
    // Given a persist and a cancel
    let persist = SagaEffect::persist(1u32).with_schedules(vec![ScheduleSpec {
        name: TimerName::new("keep"),
        after: Duration::from_secs(1),
        payload: Bytes::new(),
    }]);
    let cancel = SagaEffect::<u32>::cancel_schedule(TimerName::new("drop"));

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
fn cancel_schedule_survives_flatten_unchanged() {
    // Given a nested sequence around a CancelSchedule
    let nested = SagaEffect::<u32>::Sequence(vec![
        SagaEffect::None,
        SagaEffect::Sequence(vec![SagaEffect::cancel_schedule(TimerName::new("x"))]),
    ]);

    // When flattened
    let flat = nested.flatten();

    // Then the lone CancelSchedule is unwrapped intact (not swallowed like None)
    match flat {
        SagaEffect::CancelSchedule(name) => assert_eq!(name, TimerName::new("x")),
        _ => panic!("flatten must unwrap the single CancelSchedule leaf unchanged"),
    }
}

#[test]
#[should_panic]
fn with_schedules_on_none_panics() {
    let _ = SagaEffect::<u32>::empty().with_schedules(vec![ScheduleSpec {
        name: TimerName::new("x"),
        after: Duration::from_secs(1),
        payload: Bytes::new(),
    }]);
}

#[test]
#[should_panic]
fn with_schedules_on_cancel_variant_panics() {
    // with_schedules is defined only on Persist; a CancelSchedule is not Persist.
    let _ = SagaEffect::<u32>::cancel_schedule(TimerName::new("x")).with_schedules(vec![
        ScheduleSpec {
            name: TimerName::new("y"),
            after: Duration::from_secs(1),
            payload: Bytes::new(),
        },
    ]);
}
