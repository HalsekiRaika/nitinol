//! Scheduler identity types (Issue #50, E-28).

use crate::id::SagaId;

/// The per-saga logical key for a schedule.
///
/// Re-scheduling the same `TimerName` within a saga replaces the earlier timer
/// (Pekko `startSingleTimer` semantics), and cancellation is name-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimerName(String);

impl TimerName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The globally-unique handle a [`crate::SchedulerProxy`] uses to key a
/// registered timer.
///
/// Distinguished by the pair `(saga_id, name)`: two schedules in different
/// sagas may share a [`TimerName`] without colliding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScheduleToken {
    pub saga_id: SagaId,
    pub name: TimerName,
}
