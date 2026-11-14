use std::time::Duration;

use bytes::Bytes;

use crate::scheduler::TimerName;

/// A declarative description of effects produced by a [`crate::Saga::handle`]
/// call.
///
/// - A saga persists decisions and sets or cancels its own deadlines, and
///   nothing else. There is no "send this command": what a decision means for
///   everybody else is [`crate::Saga::dispatch`], carried out by the delivery
///   host ([`crate::SagaDeliveryProps`]).
/// - `SagaEffect<E>` is a monoid: [`SagaEffect::None`] is the identity and
///   [`SagaEffect::combine`] the associative operation. `combine` keeps a
///   `Sequence` flat and folds adjacent `Persist`s into one atomic batch.
/// - A `Sequence` built by hand gets the same `Persist` merge, because the
///   interpreter re-applies it before interpreting any effect.
pub enum SagaEffect<E> {
    /// The identity of the monoid. Writes no batch of its own; see
    /// [`crate::Saga::handle`] for the consumed-position-only append the
    /// interpreter still commits.
    None,
    /// Append `events` together with one `ScheduleEvent::Scheduled` marker per
    /// schedule, in a single atomic batch on the saga's own stream, then arm
    /// each schedule with the resident
    /// [`SchedulerProcess`](crate::SchedulerProcess). A timer never precedes
    /// the fact it stands for. With both lists empty this takes the same path
    /// as `None`.
    Persist {
        /// The saga's own events to append.
        events: Vec<E>,
        /// The deadlines to record in the same batch and arm once it lands.
        schedules: Vec<ScheduleSpec>,
    },
    /// Stop the saga process and tear down its upstream subscriptions. Effects
    /// after `End` inside a `Sequence` are not interpreted.
    End,
    /// Effects interpreted left to right, stopping at `End`.
    Sequence(Vec<SagaEffect<E>>),
    /// Cancel the timer registered under a [`TimerName`] for this saga: append
    /// a `ScheduleEvent::Cancelled` marker and cancel the pending timer. Built
    /// by [`Deadline::cancel`](crate::Deadline::cancel).
    CancelSchedule(TimerName),
}

/// A timer registration carried inside [`SagaEffect::Persist`].
///
/// Obtained only from [`Deadline::set`](crate::Deadline::set); it has no public
/// constructor and no public fields, so a saga can read what deadline an effect
/// carries but cannot invent one. The delay is relative to the moment the
/// schedule fact is written.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleSpec {
    pub(crate) name: TimerName,
    pub(crate) after: Duration,
    pub(crate) message_type: &'static str,
    pub(crate) message_codec: &'static str,
    pub(crate) payload: Bytes,
}

impl ScheduleSpec {
    pub(crate) fn new(
        name: TimerName,
        after: Duration,
        message_type: &'static str,
        message_codec: &'static str,
        payload: Bytes,
    ) -> Self {
        Self {
            name,
            after,
            message_type,
            message_codec,
            payload,
        }
    }

    /// The timer this registration is for.
    pub fn name(&self) -> &TimerName {
        &self.name
    }

    /// How far out the deadline is, measured from the moment the fact is
    /// written.
    pub fn after(&self) -> Duration {
        self.after
    }
}
