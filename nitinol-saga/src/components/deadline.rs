//! `Deadline<M>` — a saga's own time, as two effect constructors and one view.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use std::time::Duration;

use nitinol_eventsource::codec::Codec;

use crate::effect::{SagaEffect, ScheduleSpec};
use crate::scheduler::TimerName;

/// A value a saga promises to its future self.
///
/// - Not an [`Event`](nitinol_eventsource::Event): it is never a row of the
///   saga's stream, only a value persisted inside a schedule fact, so it names
///   its [`Codec`] without claiming an event type.
/// - [`TYPE_NAME`](Self::TYPE_NAME) and [`CODEC_NAME`](Self::CODEC_NAME) are
///   written beside the payload, so the fact says what it holds. A firing whose
///   names do not match the current declaration is refused, not guessed at.
pub trait DeadlineMessage: Sized + Send + Sync + 'static {
    /// The name this message type claims on the wire.
    ///
    /// One per saga type: every saga type's schedule facts share one event
    /// type, and this name is what the deadline index tells a saga type's own
    /// from — see [`DeclaresDeadline::MESSAGE_TYPE`](crate::DeclaresDeadline::MESSAGE_TYPE).
    const TYPE_NAME: &'static str;
    /// The name of the format [`Self::Codec`] writes this message as.
    const CODEC_NAME: &'static str;
    /// The format this message is written into the schedule fact as.
    type Codec: Codec<Self>;
}

/// One deadline the journal says is still pending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDeadline {
    /// The timer this deadline is registered under.
    pub name: TimerName,
    /// When it is due: the schedule fact's wall-clock anchor plus its delay.
    pub due_at: jiff::Timestamp,
}

/// The pending deadlines of one saga instance, by the instant each is due.
///
/// Read by the saga's [`Deadline`] component and written by the saga process as
/// the journal says so — replay's unfired schedules, then each `Scheduled` /
/// `Cancelled` / `Fired` marker that becomes durable.  A view fed by intent
/// instead would disagree with the stream exactly where it matters: after a
/// crash between asking for a deadline and the fact landing.
///
/// Cheap-clone, and a clone is another name for the one view: that is what lets
/// the process and the component it hands to the saga each hold one by value
/// and still be writing and reading the same entries.  One of these per
/// incarnation — a restarted incarnation starts from an empty one, which replay
/// then fills.
#[derive(Clone)]
pub(crate) struct PendingDeadlines {
    inner: Arc<RwLock<HashMap<TimerName, jiff::Timestamp>>>,
}

impl PendingDeadlines {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Fold what the journal says about one timer into the view.
    ///
    /// Called only after the marker that says it has actually landed, so the
    /// view follows the stream rather than the intent.
    pub(crate) fn record(&self, name: TimerName, due_at: jiff::Timestamp) {
        self.write().insert(name, due_at);
    }

    /// Drop a timer from the view: its `cancelled` or `fired` marker is
    /// durable.
    pub(crate) fn forget(&self, name: &TimerName) {
        self.write().remove(name);
    }

    /// Replace the view with what replay found on the stream.
    ///
    /// A fresh incarnation knows nothing of what it set, so the journal is the
    /// whole of the answer — which is also why this replaces rather than
    /// merges.
    pub(crate) fn replay(&self, pending: impl IntoIterator<Item = (TimerName, jiff::Timestamp)>) {
        let mut held = self.write();
        held.clear();
        held.extend(pending);
    }

    /// What the view holds, in due order.
    pub(crate) fn pending(&self) -> Vec<PendingDeadline> {
        let held = self
            .inner
            .read()
            .expect("the pending-deadline view is never held across a panic");
        let mut pending: Vec<PendingDeadline> = held
            .iter()
            .map(|(name, due_at)| PendingDeadline {
                name: name.clone(),
                due_at: *due_at,
            })
            .collect();
        // A map has no order of its own, and a caller comparing two reads of the
        // same journal state must get the same answer twice.
        pending.sort_by(|left, right| {
            left.due_at
                .cmp(&right.due_at)
                .then_with(|| left.name.as_str().cmp(right.name.as_str()))
        });
        pending
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<TimerName, jiff::Timestamp>> {
        self.inner
            .write()
            .expect("the pending-deadline view is never held across a panic")
    }
}

/// A saga's own time: constructors for the effects that set and cancel a
/// deadline, and a read-only view of what the journal says is pending.
///
/// - Obtained only from [`SagaComponents::deadline`](crate::SagaComponents::deadline),
///   inside the producer the spawn builder calls.
/// - It holds no scheduler handle: a deadline is set by a [`SagaEffect`] that
///   appends the schedule fact before the timer is armed.
/// - A firing reaches the deadline arm of [`Saga::handle`](crate::Saga::handle)
///   and is settled by a `fired` fact once what it decided is durable; until
///   then it stays pending and fires again on a later activation, so an arm
///   must tolerate a repeat. A firing that decides nothing or returns `Err`, and
///   one whose schedule fact names a message the alphabet does not declare, is
///   settled the same way; one the saga ends on, or whose arm re-sets the same [`TimerName`], is
///   closed by the `Ended` fact or the new schedule fact instead.
pub struct Deadline<M> {
    pending: PendingDeadlines,
    /// `fn() -> M` rather than `M`, so the component is `Send + Sync` whatever
    /// the message is and holds no claim on owning one.
    _message: PhantomData<fn() -> M>,
}

impl<M> Deadline<M> {
    pub(crate) fn new(pending: PendingDeadlines) -> Self {
        Self {
            pending,
            _message: PhantomData,
        }
    }
}

impl<M: DeadlineMessage> Deadline<M> {
    /// Set a deadline `after` from now, keyed by `name`, carrying `message`.
    ///
    /// Re-using `name` replaces the earlier deadline, with no cancel needed.
    ///
    /// # Panics
    ///
    /// Panics if `M::Codec` fails to encode `message`.  A deadline whose
    /// message cannot be written has no silent reading: dropping it would lose
    /// the promise, and reporting it as a domain error would oblige every saga
    /// to convert a framework fault into its own error type.
    pub fn set<E>(&self, name: TimerName, after: Duration, message: M) -> SagaEffect<E> {
        let payload = nitinol_eventsource::codec::encode::<M::Codec, M>(&message).expect(
            "Deadline::set: the deadline message's own codec failed to encode it; \
             ensure the type's `DeadlineMessage::Codec` encodes every value it accepts",
        );
        SagaEffect::Persist {
            events: Vec::new(),
            schedules: vec![ScheduleSpec::new(
                name,
                after,
                M::TYPE_NAME,
                M::CODEC_NAME,
                payload,
            )],
        }
    }

    /// Cancel the deadline registered under `name`.
    ///
    /// An effect for the same reason [`Deadline::set`] is: the `cancelled` fact
    /// is what a later replay reads to *not* re-arm the deadline, so cancelling
    /// the live timer without writing it would leave the two disagreeing.
    pub fn cancel<E>(&self, name: TimerName) -> SagaEffect<E> {
        SagaEffect::CancelSchedule(name)
    }

    /// The deadlines the journal says are still pending: set, and neither fired
    /// nor cancelled.
    ///
    /// Restored by replay from the saga's own stream and moved by each schedule
    /// marker that becomes durable — never by a call to [`Deadline::set`] whose
    /// append has not landed, and never by a firing arriving.  A deadline leaves
    /// this view when its own `fired` marker is durable, or when a schedule fact
    /// of the same name replaces it; a firing whose decision could not be
    /// persisted leaves it right here, which is also what replay would read off
    /// the stream.  So the view and the stream cannot disagree.
    pub fn pending(&self) -> Vec<PendingDeadline> {
        self.pending.pending()
    }
}

/// When a deadline recorded with `after` from the wall-clock anchor
/// `anchor_unix_millis` is due.
///
/// The schedule fact persists a relative delay and the anchor it was measured
/// from rather than an absolute instant, so that a replay can recompute the
/// remaining wait.  The pending view wants the absolute answer, and deriving it
/// here is what keeps the view and the re-armed timer reading the same fact the
/// same way.
///
/// Saturating throughout: a delay past the representable range clamps to the
/// furthest instant rather than wrapping into the past, which would report a
/// deadline as overdue.
pub(crate) fn due_at(anchor_unix_millis: i64, after: Duration) -> jiff::Timestamp {
    let after_millis = i64::try_from(after.as_millis()).unwrap_or(i64::MAX);
    let due = anchor_unix_millis.saturating_add(after_millis);
    jiff::Timestamp::from_millisecond(due).unwrap_or(jiff::Timestamp::MAX)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    /// A deadline message whose codec is not a serde format, so
    /// "written through the codec the type named" is distinguishable from
    /// "written through the framework's own format".
    #[derive(Clone, Debug, PartialEq)]
    struct Reminder {
        note: String,
    }

    struct ReminderCodec;

    impl Codec<Reminder> for ReminderCodec {
        type Error = std::convert::Infallible;

        fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
            Ok(Bytes::from(format!("note:{}", value.note).into_bytes()))
        }

        fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
            Ok(Reminder {
                note: String::from_utf8_lossy(payload)
                    .trim_start_matches("note:")
                    .to_owned(),
            })
        }
    }

    impl DeadlineMessage for Reminder {
        const TYPE_NAME: &'static str = "reminder";
        const CODEC_NAME: &'static str = "reminder-codec";
        type Codec = ReminderCodec;
    }

    /// A component over an empty journal view.
    ///
    /// The constructors under test read none of it — which is itself part of the
    /// contract: what `set` builds is a decision, not a record of one.
    fn component() -> Deadline<Reminder> {
        Deadline::new(PendingDeadlines::new())
    }

    fn schedules<E>(effect: &SagaEffect<E>) -> &[ScheduleSpec] {
        match effect {
            SagaEffect::Persist { schedules, .. } => schedules,
            _ => panic!("`Deadline::set` must build a `Persist` branch"),
        }
    }

    /// `set` retains the name and the *relative* delay it was given, and writes
    /// the message through the codec its own type named.
    #[test]
    fn set_retains_name_and_delay_and_encodes_through_the_messages_codec() {
        let name = TimerName::new("reminder");
        let after = Duration::from_secs(30);
        let message = Reminder {
            note: "ping".to_owned(),
        };

        let effect = component().set::<u32>(name.clone(), after, message.clone());

        let schedules = schedules(&effect);
        assert_eq!(
            schedules.len(),
            1,
            "`set` must build exactly one schedule entry"
        );
        assert_eq!(
            schedules[0].name(),
            &name,
            "the schedule fact must name the timer it was asked for"
        );
        assert_eq!(
            schedules[0].after(),
            after,
            "the delay must stay relative, so a replay can recompute the \
             remaining wait from the persisted anchor"
        );
        assert_eq!(
            schedules[0].payload,
            Bytes::from_static(b"note:ping"),
            "the message must be written through `DeadlineMessage::Codec`, not \
             through a format the framework picked"
        );
        assert_eq!(
            schedules[0].message_type,
            Reminder::TYPE_NAME,
            "the schedule fact must name the message type it carries, not \
             leave it to be inferred from whichever alphabet reads it back"
        );
        assert_eq!(
            schedules[0].message_codec,
            Reminder::CODEC_NAME,
            "the schedule fact must name the codec `payload` was written with"
        );
    }

    /// Setting a deadline is not a decision about anything else: the branch
    /// carries the schedule alone.
    ///
    /// Asserted on the effect rather than on a stream because a spurious event
    /// smuggled in here would be appended in the decision's own atomic batch
    /// and read as the saga's doing.
    #[test]
    fn set_adds_no_user_event() {
        let effect = component().set::<u32>(
            TimerName::new("reminder"),
            Duration::from_secs(1),
            Reminder {
                note: "x".to_owned(),
            },
        );

        match &effect {
            SagaEffect::Persist { events, schedules } => {
                assert!(events.is_empty(), "`set` must persist no user event");
                assert_eq!(schedules.len(), 1);
            }
            _ => panic!("`Deadline::set` must build a `Persist` branch"),
        }
    }

    /// `cancel` is the name-scoped cancellation, carrying the name it was given.
    #[test]
    fn cancel_builds_the_cancel_branch_for_the_named_timer() {
        let name = TimerName::new("reminder");

        match component().cancel::<u32>(name.clone()) {
            SagaEffect::CancelSchedule(carried) => assert_eq!(
                carried, name,
                "`cancel(name)` must carry the timer name it was asked to drop"
            ),
            _ => panic!("`Deadline::cancel` must build `SagaEffect::CancelSchedule`"),
        }
    }

    /// A deadline combines with the rest of a decision as one flat sequence, in
    /// the order written — the atomic-batch guarantee is stated on the
    /// interpreter, and depends on the leaves surviving `combine` unnested.
    #[test]
    fn a_schedule_combines_with_a_decision_preserving_order() {
        let deadline = component();
        let persist = SagaEffect::persist(1u32).combine(deadline.set(
            TimerName::new("keep"),
            Duration::from_secs(1),
            Reminder {
                note: "keep".to_owned(),
            },
        ));

        match persist.combine(deadline.cancel(TimerName::new("drop"))) {
            SagaEffect::Sequence(children) => {
                assert_eq!(children.len(), 2, "combine must not nest or drop leaves");
                assert!(matches!(children[0], SagaEffect::Persist { .. }));
                assert!(matches!(children[1], SagaEffect::CancelSchedule(_)));
            }
            _ => panic!("a decision combined with a cancellation must be a `Sequence`"),
        }
    }
}
