//! Scheduler identity types.

use crate::id::SagaId;

/// The per-saga logical key for a schedule.
///
/// Re-scheduling the same `TimerName` within a saga replaces the earlier timer
/// (Pekko `startSingleTimer` semantics), and cancellation is name-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimerName(String);

impl TimerName {
    /// Creates a timer name from a string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns the name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The globally-unique handle the resident timer core uses to key a registered
/// timer.
///
/// Distinguished by the pair `(saga_id, name)`: two schedules in different
/// sagas may share a [`TimerName`] without colliding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScheduleToken {
    /// The saga the timer belongs to.
    pub saga_id: SagaId,
    /// The timer's name within that saga.
    pub name: TimerName,
}

impl ScheduleToken {
    /// The handle addressing `name` within `saga_id`.
    ///
    /// Registration, cancellation, firing and replay all address the same
    /// timer, so the pair is settled here rather than paired again at each of
    /// them.
    pub fn new(saga_id: SagaId, name: TimerName) -> Self {
        Self { saga_id, name }
    }
}
