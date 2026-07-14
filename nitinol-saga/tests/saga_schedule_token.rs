//! Tests for the scheduler identity types (Issue #50, E-28):
//! - `TimerName` — the per-saga logical key for a schedule.
//! - `ScheduleToken { saga_id, name }` — the globally-unique handle a
//!   `SchedulerProcess` uses to key a registered timer.
//!
//! A `ScheduleToken` is distinguished by the pair `(saga_id, name)`: two
//! schedules in different sagas may share a `TimerName` without colliding, and
//! within one saga two distinct `TimerName`s address distinct timers.

use nitinol_saga::{ScheduleToken, SagaId, TimerName};

#[test]
fn timer_names_compare_by_value() {
    // Given two names built from the same text and one from different text
    let a = TimerName::new("reminder");
    let a_again = TimerName::new("reminder");
    let b = TimerName::new("deadline");

    // Then equality is by value
    assert_eq!(a, a_again, "equal text must yield equal TimerNames");
    assert_ne!(a, b, "different text must yield different TimerNames");
}

#[test]
fn schedule_token_holds_saga_id_and_name() {
    // Given a saga id and a timer name
    let saga_id = SagaId::new("order-42");
    let name = TimerName::new("reminder");

    // When a token pairs them
    let token = ScheduleToken {
        saga_id: saga_id.clone(),
        name: name.clone(),
    };

    // Then both components are retained
    assert_eq!(token.saga_id, saga_id, "token must retain its SagaId");
    assert_eq!(token.name, name, "token must retain its TimerName");
}

#[test]
fn tokens_are_equal_only_when_both_saga_id_and_name_match() {
    let base = ScheduleToken {
        saga_id: SagaId::new("order-1"),
        name: TimerName::new("reminder"),
    };
    let same = ScheduleToken {
        saga_id: SagaId::new("order-1"),
        name: TimerName::new("reminder"),
    };
    let other_saga = ScheduleToken {
        saga_id: SagaId::new("order-2"),
        name: TimerName::new("reminder"),
    };
    let other_name = ScheduleToken {
        saga_id: SagaId::new("order-1"),
        name: TimerName::new("deadline"),
    };

    assert_eq!(base, same, "same (saga_id, name) pair must be equal");
    assert_ne!(
        base, other_saga,
        "same name in a different saga must be a distinct token"
    );
    assert_ne!(
        base, other_name,
        "a different name in the same saga must be a distinct token"
    );
}
