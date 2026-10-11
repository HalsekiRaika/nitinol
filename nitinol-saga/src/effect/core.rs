use std::time::Duration;

use bytes::Bytes;

use crate::scheduler::TimerName;

/// A declarative description of effects produced by a [`crate::Saga::handle`]
/// call.
///
/// `SagaEffect<E>` is a Monoid: [`SagaEffect::None`] is the identity element
/// and [`SagaEffect::combine`] is the associative binary operation.  The
/// [`SagaEffect::Sequence`] variant accumulates leaves in a flat list;
/// `combine` introduces no nesting and folds an adjacent `Persist × Persist`
/// junction into one `Persist` so the pair is written as a single atomic batch.
///
/// # What a saga can express
///
/// A saga persists decisions and sets its own deadlines, and nothing else.
/// There is no way to express "send this command": a decision's consequences
/// for everybody else are the pure mapping [`crate::Saga::dispatch`] answers,
/// and carrying them is the delivery host's work — see
/// [`crate::SagaDeliveryProps`] for what that delivers and how often.
///
/// # Variants
///
/// - `None` — Monoid identity.  Writes no batch of its own on the saga's own
///   event store; see [`crate::Saga::handle`] for when the interpreter still
///   commits a consumed-position-only append for the input this was decided
///   from.
/// - `Persist { events, schedules }` — append the user events together with one
///   `ScheduleEvent::Scheduled` marker per schedule, **in a single atomic
///   batch** on the saga's own event store.  After the append succeeds each
///   schedule is armed with the resident
///   [`SchedulerProcess`](crate::SchedulerProcess) the host resolved.
///   Appending before arming is the whole order that matters: a timer can never
///   precede the fact it stands for.
///   A branch whose `events` and `schedules` are both empty writes no batch of
///   its own either, taking the same path as `None` — see
///   [`crate::Saga::handle`] for the full contract.
/// - `End` — single-responsibility termination marker.  Stops the saga
///   process and tears down its upstream subscriptions.  Effects placed after
///   `End` inside a `Sequence` are not interpreted.
/// - `Sequence(Vec<SagaEffect<E>>)` — Monoid composition.  Interpreted left
///   to right with short-circuit on `End`.  All variants are public, so a
///   `Sequence` can be hand-built without going through `combine`; the
///   interpreter re-applies `combine`'s adjacent `Persist × Persist` merge to
///   every effect before interpreting it, so the single-atomic-batch
///   guarantee documented on `Persist` holds regardless of how the effect was
///   constructed.
/// - `CancelSchedule(TimerName)` — name-scoped timer cancellation.  Persists a
///   `ScheduleEvent::Cancelled` marker and cancels the pending timer.  Built by
///   [`Deadline::cancel`](crate::Deadline::cancel).
pub enum SagaEffect<E> {
    /// The identity element of the Monoid.  See type-level docs for the full
    /// contract.
    None,
    /// Persist a batch of user events together with the per-schedule
    /// `ScheduleEvent::Scheduled` markers in one atomic `store::append` call.
    /// See type-level docs for the full contract.
    Persist {
        events: Vec<E>,
        schedules: Vec<ScheduleSpec>,
    },
    /// Stop the saga process.  The runtime cascade-stops every upstream
    /// `DirectPollerProcess` child — one per declared source — preventing
    /// further `handle` calls.
    End,
    /// An ordered collection of effects executed sequentially.
    Sequence(Vec<SagaEffect<E>>),
    /// Cancel the timer registered under a [`TimerName`] for this saga.
    /// Interpreted as a `ScheduleEvent::Cancelled` append plus a
    /// scheduler cancel; equivalent to token-scoped cancel since the saga id is
    /// fixed to the emitting saga.
    CancelSchedule(TimerName),
}

/// A timer registration carried inside a `Persist` branch.
///
/// The interpreter appends a `ScheduleEvent::Scheduled` marker as part of the
/// same atomic batch as the user events, then registers the timer with the
/// resident scheduler.  The delay is relative (`after: Duration`), not an
/// absolute instant, so it survives replay via a persisted wall-clock anchor.
/// `payload` is the deadline message written through the
/// [`Codec`](nitinol_eventsource::codec::Codec) its own type named, and
/// `message_type` / `message_codec` are that same type's
/// [`DeadlineMessage::TYPE_NAME`](crate::DeadlineMessage::TYPE_NAME) /
/// [`DeadlineMessage::CODEC_NAME`](crate::DeadlineMessage::CODEC_NAME),
/// carried beside it so the schedule fact says what `payload` is.
///
/// # Why there is no public constructor
///
/// [`Deadline::set`](crate::Deadline::set) is the only way to build one, and a
/// [`Deadline`](crate::Deadline) is only obtainable from the producer the spawn
/// builder calls.  So a saga that did not ask for its own time has no way to
/// name a timer at all, and the arm of its alphabet that would receive one is a
/// visible dead branch rather than a silent misconfiguration.
///
/// The type stays public because it appears inside [`SagaEffect::Persist`],
/// whose variants are public: a caller that matches an effect can read what
/// deadline it carries, and still cannot invent one.
///
/// The fields are private, so naming a timer by writing the value down is a
/// compile error:
///
/// ```compile_fail
/// # use std::time::Duration;
/// # use bytes::Bytes;
/// # use nitinol_saga::{ScheduleSpec, TimerName};
/// let _ = ScheduleSpec {
///     name: TimerName::new("reminder"),
///     after: Duration::from_secs(30),
///     message_type: "reminder",
///     message_codec: "reminder-codec",
///     payload: Bytes::new(),
/// };
/// ```
///
/// And so is reaching for the builder this type used to have, which took a
/// deadline message the framework serialized itself:
///
/// ```compile_fail
/// # use std::time::Duration;
/// # use nitinol_saga::{SagaEffect, TimerName};
/// let _: SagaEffect<u32> =
///     SagaEffect::schedule(TimerName::new("reminder"), Duration::from_secs(30), ());
/// ```
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
