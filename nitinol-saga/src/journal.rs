//! Pure projection of the saga's own event stream.
//!
//! `JournalState` folds the persisted `SagaPersisted` envelope into the facts
//! replay needs — highest observed sequence, termination, active schedules,
//! consumed upstream positions — without performing any I/O.  Loading the
//! stream and acting on the projection stay in `process::replay`.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;

use crate::consumed::ConsumedPositions;
use crate::lifecycle::LifecycleEvent;
use crate::persisted::SagaPersisted;
use crate::scheduler::{ScheduleEvent, TimerName};

/// A schedule the journal found still active — a persisted `Scheduled` with no
/// later `Cancelled` / `Fired` for the same [`TimerName`].  Re-registered with
/// the resident scheduler on `on_start`.
pub(crate) struct ActiveSchedule {
    pub(crate) name: TimerName,
    pub(crate) after: Duration,
    /// The identity `payload` was persisted under — see
    /// [`crate::scheduler::ScheduleEvent::Scheduled`] for why the fact carries
    /// it rather than leaving it to the reader's compiled-in alphabet.
    pub(crate) message_type: String,
    pub(crate) message_codec: String,
    pub(crate) payload: Bytes,
    pub(crate) scheduled_at_unix_millis: i64,
}

/// The facts replay needs, folded out of the saga's own event stream.
///
/// The fold performs no I/O and makes no decision about re-registration: it
/// reports what the stream said, and `process::replay` decides what to do about
/// it.
pub(crate) struct JournalState {
    /// Highest stream sequence observed, never below the caller's floor.
    pub(crate) sequence: u64,
    /// `true` once the stream carries a durable `Ended` lifecycle fact — the
    /// saga terminated in a previous incarnation.
    pub(crate) ended: bool,
    pub(crate) active_schedules: HashMap<TimerName, ActiveSchedule>,
    /// How far each upstream source has been consumed, rebuilt from the
    /// positions the stream carries.
    pub(crate) consumed: ConsumedPositions,
}

impl JournalState {
    /// Start a projection whose sequence floor is `from_sequence` — the
    /// position the caller has already applied and loads events after.
    pub(crate) fn new(from_sequence: u64) -> Self {
        Self {
            sequence: from_sequence,
            ended: false,
            active_schedules: HashMap::new(),
            consumed: ConsumedPositions::default(),
        }
    }

    /// Record the stream position of a loaded entry.  Kept apart from
    /// [`Self::fold`] because the position is a property of the storage record,
    /// not of the folded fact.
    pub(crate) fn observe_sequence(&mut self, sequence: u64) {
        self.sequence = self.sequence.max(sequence);
    }

    /// Fold one persisted entry into the projection, handing the domain event
    /// back when the entry carries one.  Applying it belongs to the caller,
    /// which keeps this fold independent of the saga's state type.
    pub(crate) fn fold<E>(&mut self, persisted: SagaPersisted<E>) -> Option<E> {
        match persisted {
            SagaPersisted::Domain(event) => return Some(event),
            SagaPersisted::Schedule(marker) => self.fold_schedule(marker),
            SagaPersisted::Consumed(position) => self.consumed.record(
                position.source.as_str(),
                position.source_event_type,
                position.sequence,
            ),
            SagaPersisted::Lifecycle(LifecycleEvent::Ended) => self.ended = true,
        }
        None
    }

    /// Timers are keyed by [`TimerName`], so re-scheduling a name supersedes
    /// the earlier timer and cancellation is name-scoped.
    fn fold_schedule(&mut self, marker: ScheduleEvent) {
        match marker {
            ScheduleEvent::Scheduled {
                token,
                after,
                message_type,
                message_codec,
                payload,
                scheduled_at_unix_millis,
            } => {
                self.active_schedules.insert(
                    token.name.clone(),
                    ActiveSchedule {
                        name: token.name,
                        after,
                        message_type,
                        message_codec,
                        payload,
                        scheduled_at_unix_millis,
                    },
                );
            }
            ScheduleEvent::Cancelled { token, .. } | ScheduleEvent::Fired { token, .. } => {
                self.active_schedules.remove(&token.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use nitinol_persistence::AggregateId;

    use super::*;

    use crate::consumed::{ConsumedPositionEvent, PositionSource};
    use crate::id::SagaId;
    use crate::lifecycle::LifecycleEvent;
    use crate::persisted::SagaPersisted;
    use crate::scheduler::{ScheduleEvent, ScheduleToken, TimerName};

    /// Stand-in for the saga's domain event.  `fold` carries no trait bound on
    /// `E`, so no `Event` / codec implementation is needed here.
    #[derive(Debug, PartialEq)]
    struct DomainEvt(u32);

    fn saga_id() -> SagaId {
        SagaId::new("journal-unit-test")
    }

    fn token(name: &str) -> ScheduleToken {
        ScheduleToken {
            saga_id: saga_id(),
            name: TimerName::new(name),
        }
    }

    fn ended() -> SagaPersisted<DomainEvt> {
        SagaPersisted::Lifecycle(LifecycleEvent::Ended)
    }

    /// The identity every `scheduled()` fixture in this module carries — the
    /// tests here are about the fold, not about identity variance, so one
    /// fixed pair stands in for whatever `DeadlineMessage` a real saga names.
    const MESSAGE_TYPE: &str = "reminder";
    const MESSAGE_CODEC: &str = "reminder-codec";

    fn scheduled(
        name: &str,
        after: Duration,
        payload: &'static [u8],
        scheduled_at_unix_millis: i64,
    ) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Schedule(ScheduleEvent::Scheduled {
            token: token(name),
            after,
            message_type: MESSAGE_TYPE.to_owned(),
            message_codec: MESSAGE_CODEC.to_owned(),
            payload: Bytes::from_static(payload),
            scheduled_at_unix_millis,
        })
    }

    fn cancelled(name: &str) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Schedule(ScheduleEvent::Cancelled {
            token: token(name),
            cancelled_at_unix_millis: 0,
        })
    }

    /// The declared source every position in these tests belongs to.  The fold
    /// keys on it, so naming one source throughout keeps the assertions about
    /// *which stream* a position covers rather than which source.
    fn folded_source() -> PositionSource {
        PositionSource::of(nitinol_persistence::EventType::new(
            nitinol_persistence::Family::new("journal_test"),
            nitinol_persistence::TypeName::new("Observed"),
        ))
    }

    fn consumed_position(source: &str, sequence: u64) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Consumed(ConsumedPositionEvent {
            source: AggregateId::new(source),
            source_event_type: folded_source(),
            sequence,
        })
    }

    fn fired(name: &str) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Schedule(ScheduleEvent::Fired {
            token: token(name),
            fired_at_unix_millis: 0,
        })
    }

    #[test]
    fn the_ended_fact_marks_the_journal_ended() {
        let mut journal = JournalState::new(0);

        let handed_back = journal.fold(ended());

        assert!(
            handed_back.is_none(),
            "a lifecycle fact is folded into the journal, never handed back as a domain event"
        );
        assert!(
            journal.ended,
            "a durable Ended fact means the saga terminated in an earlier incarnation"
        );
    }

    #[test]
    fn domain_event_is_handed_back_to_the_caller() {
        let mut journal = JournalState::new(4);

        let handed_back = journal.fold(SagaPersisted::Domain(DomainEvt(9)));

        assert_eq!(
            handed_back,
            Some(DomainEvt(9)),
            "a domain event must be handed back so the caller applies it in stream order"
        );
        assert_eq!(
            journal.sequence, 4,
            "folding an event must not move the sequence on its own"
        );
        assert!(
            journal.active_schedules.is_empty() && !journal.ended,
            "a domain event carries no journal facts and must leave the projection untouched"
        );
    }

    #[test]
    fn scheduled_marker_registers_an_active_schedule_under_its_timer_name() {
        let mut journal = JournalState::new(0);

        journal.fold(scheduled(
            "deadline",
            Duration::from_millis(250),
            b"payload",
            1_700_000_000_123,
        ));

        let active = journal
            .active_schedules
            .get(&TimerName::new("deadline"))
            .expect(
                "a Scheduled marker must be keyed by its timer name, not by the schedule token",
            );
        assert_eq!(active.name, TimerName::new("deadline"));
        assert_eq!(
            active.after,
            Duration::from_millis(250),
            "the original delay must survive replay so re-registration is faithful"
        );
        assert_eq!(
            active.payload,
            Bytes::from_static(b"payload"),
            "the scheduled payload must survive replay"
        );
        assert_eq!(
            active.message_type, MESSAGE_TYPE,
            "the message's identity must survive replay alongside its payload"
        );
        assert_eq!(
            active.message_codec, MESSAGE_CODEC,
            "the codec's identity must survive replay alongside the payload"
        );
        assert_eq!(
            active.scheduled_at_unix_millis, 1_700_000_000_123,
            "the original time anchor must survive replay, not be re-taken at replay time"
        );
    }

    #[test]
    fn rescheduling_the_same_timer_name_supersedes_the_earlier_schedule() {
        let mut journal = JournalState::new(0);

        journal.fold(scheduled(
            "deadline",
            Duration::from_millis(100),
            b"first",
            1,
        ));
        journal.fold(scheduled(
            "deadline",
            Duration::from_millis(900),
            b"second",
            2,
        ));

        assert_eq!(
            journal.active_schedules.len(),
            1,
            "re-scheduling a timer name replaces the earlier timer instead of adding one"
        );
        let active = journal
            .active_schedules
            .get(&TimerName::new("deadline"))
            .expect("the timer name must still be registered after re-scheduling");
        assert_eq!(
            active.after,
            Duration::from_millis(900),
            "the later schedule wins; the earlier one must not survive"
        );
        assert_eq!(active.payload, Bytes::from_static(b"second"));
        assert_eq!(active.scheduled_at_unix_millis, 2);
    }

    #[test]
    fn cancelled_marker_removes_the_active_schedule() {
        let mut journal = JournalState::new(0);
        journal.fold(scheduled("deadline", Duration::from_millis(100), b"p", 1));

        journal.fold(cancelled("deadline"));

        assert!(
            journal.active_schedules.is_empty(),
            "a cancelled timer must not be re-registered on restart"
        );
    }

    #[test]
    fn fired_marker_removes_the_active_schedule() {
        let mut journal = JournalState::new(0);
        journal.fold(scheduled("deadline", Duration::from_millis(100), b"p", 1));

        journal.fold(fired("deadline"));

        assert!(
            journal.active_schedules.is_empty(),
            "an already fired timer must not be re-registered on restart"
        );
    }

    #[test]
    fn cancel_for_an_unknown_timer_name_is_a_no_op() {
        let mut journal = JournalState::new(0);
        journal.fold(scheduled("deadline", Duration::from_millis(100), b"p", 1));

        journal.fold(cancelled("other-timer"));

        assert!(
            journal
                .active_schedules
                .contains_key(&TimerName::new("deadline")),
            "cancellation is name-scoped and must not touch a differently named timer"
        );
        assert_eq!(journal.active_schedules.len(), 1);
    }

    #[test]
    fn ended_journal_still_reports_the_schedules_it_folded() {
        let mut journal = JournalState::new(0);
        journal.fold(scheduled("deadline", Duration::from_millis(100), b"p", 1));

        journal.fold(ended());

        assert_eq!(
            journal.active_schedules.len(),
            1,
            "the fold reports what the stream said; dropping schedules for an ended saga is replay's decision"
        );
    }

    #[test]
    fn a_consumed_position_is_folded_under_the_source_it_names() {
        let mut journal = JournalState::new(0);

        let handed_back = journal.fold(consumed_position("order-stream", 2));

        assert!(
            handed_back.is_none(),
            "a position is framework bookkeeping, not a domain event to apply"
        );
        assert!(
            journal.consumed.covers("order-stream", &folded_source(), 2),
            "the position the stream carries must be what the fold restores"
        );
        assert!(
            !journal.consumed.covers("other-stream", &folded_source(), 1),
            "a position covers the source it was read from and nothing else"
        );
        assert!(
            journal.active_schedules.is_empty() && !journal.ended,
            "a position says what was consumed and nothing more"
        );
    }

    /// The stream may hold a later position naming a lower sequence — an input
    /// redelivered while an earlier incarnation was still writing.  A fold that
    /// kept the last row it saw would re-open an input whose decision is
    /// already durable.
    #[test]
    fn a_later_lower_position_does_not_move_the_fold_backwards() {
        let mut journal = JournalState::new(0);

        journal.fold(consumed_position("order-stream", 2));
        journal.fold(consumed_position("order-stream", 1));

        assert!(
            journal.consumed.covers("order-stream", &folded_source(), 2),
            "the highest position wins, whatever order the rows are in"
        );
    }

    #[test]
    fn a_stream_with_no_position_consumed_nothing() {
        let mut journal = JournalState::new(0);

        journal.fold(scheduled("deadline", Duration::from_millis(100), b"p", 1));

        assert!(
            !journal.consumed.covers("order-stream", &folded_source(), 1),
            "with no position on the stream every upstream record is still \
             unconsumed; claiming otherwise would swallow inputs no decision \
             was ever made from"
        );
    }

    #[test]
    fn sequence_keeps_the_highest_observed_value_from_the_initial_floor() {
        let mut journal = JournalState::new(5);
        assert_eq!(
            journal.sequence, 5,
            "the sequence starts at the caller's floor, since replay only loads events after it"
        );

        journal.observe_sequence(3);
        assert_eq!(
            journal.sequence, 5,
            "a lower observation must never move the sequence backwards"
        );

        journal.observe_sequence(9);
        assert_eq!(
            journal.sequence, 9,
            "the sequence tracks the highest observed stream position"
        );

        journal.observe_sequence(7);
        assert_eq!(
            journal.sequence, 9,
            "the highest observed position must survive a later lower observation"
        );
    }
}
