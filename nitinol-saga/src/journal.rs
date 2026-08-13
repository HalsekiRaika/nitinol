//! Pure projection of the saga's own event stream.
//!
//! `JournalState` folds the persisted `SagaPersisted` envelope into the facts
//! replay needs — highest observed sequence, termination, pending tells,
//! active schedules, failed tell ids — without performing any I/O.  Loading the
//! stream and acting on the projection stay in `process::replay`.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;

use crate::id::SagaId;
use crate::outbox::OutboxEvent;
use crate::persisted::SagaPersisted;
use crate::scheduler::{ScheduleEvent, TimerName};

/// A schedule the journal found still active — a persisted `Scheduled` with no
/// later `Cancelled` / `Fired` for the same [`TimerName`].  Re-registered with
/// the resident scheduler on `on_start`.
pub(crate) struct ActiveSchedule {
    pub(crate) name: TimerName,
    pub(crate) after: Duration,
    pub(crate) payload: Bytes,
    pub(crate) scheduled_at_unix_millis: i64,
}

/// A tell the journal found unsettled — a durable `TellRequested` with no later
/// `TellAcked` / `TellFailed`.
pub(crate) struct PendingTell {
    /// The crash-restart payload, if the `TellRequested` carried one.
    pub(crate) crash_restart: Option<Bytes>,
    /// The target aggregate's stream key, if stored in the `TellRequested`
    /// (absent for events written before this field was added to the proto).
    pub(crate) target: Option<SagaId>,
}

/// The facts replay needs, folded out of the saga's own event stream.
///
/// The fold performs no I/O and makes no decision about re-dispatch or
/// re-registration: it reports what the stream said, and `process::replay`
/// decides what to do about it.
pub(crate) struct JournalState {
    /// Highest stream sequence observed, never below the caller's floor.
    pub(crate) sequence: u64,
    /// `true` once the stream carries a durable `Ended` marker — the saga
    /// terminated in a previous incarnation.
    pub(crate) ended: bool,
    pub(crate) pending_tells: HashMap<u64, PendingTell>,
    pub(crate) active_schedules: HashMap<TimerName, ActiveSchedule>,
    /// `tell_id`s whose terminal marker is `TellFailed`, in stream order.
    pub(crate) failed_tell_ids: Vec<u64>,
}

impl JournalState {
    /// Start a projection whose sequence floor is `from_sequence` — the
    /// position the caller has already applied and loads events after.
    pub(crate) fn new(from_sequence: u64) -> Self {
        Self {
            sequence: from_sequence,
            ended: false,
            pending_tells: HashMap::new(),
            active_schedules: HashMap::new(),
            failed_tell_ids: Vec::new(),
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
            SagaPersisted::Outbox(marker) => self.fold_outbox(marker),
            SagaPersisted::Schedule(marker) => self.fold_schedule(marker),
            // Dead letters are not folded into saga state — they are an
            // observability side-channel delivered to subscribers.
            SagaPersisted::DeadLetter(_) => {}
        }
        None
    }

    fn fold_outbox(&mut self, marker: OutboxEvent) {
        match marker {
            OutboxEvent::TellRequested(m) => {
                // An empty target marks a legacy entry written before the proto
                // carried the field; unknown must not become an empty id.
                let target = if m.target.is_empty() {
                    None
                } else {
                    Some(SagaId::new(&m.target))
                };
                self.pending_tells.insert(
                    m.tell_id,
                    PendingTell {
                        crash_restart: m.crash_restart.map(Bytes::from),
                        target,
                    },
                );
            }
            OutboxEvent::TellAcked(m) => {
                self.pending_tells.remove(&m.tell_id);
            }
            OutboxEvent::TellFailed(m) => {
                self.pending_tells.remove(&m.tell_id);
                self.failed_tell_ids.push(m.tell_id);
            }
            OutboxEvent::Scheduled(m) => {
                tracing::trace!(
                    at_unix_seconds = m.at_unix_seconds,
                    "saga replay: scheduled marker (no-op)"
                );
            }
            OutboxEvent::Ended(_) => {
                self.ended = true;
            }
        }
    }

    /// Timers are keyed by [`TimerName`], so re-scheduling a name supersedes
    /// the earlier timer and cancellation is name-scoped.
    fn fold_schedule(&mut self, marker: ScheduleEvent) {
        match marker {
            ScheduleEvent::Scheduled {
                token,
                after,
                payload,
                scheduled_at_unix_millis,
            } => {
                self.active_schedules.insert(
                    token.name.clone(),
                    ActiveSchedule {
                        name: token.name,
                        after,
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

    use super::*;

    use crate::dead_letter::{DeadLetterEvent, SagaFailure, SourceContext};
    use crate::id::SagaId;
    use crate::outbox::{
        Ended, OutboxEvent, Scheduled as OutboxScheduled, TellAcked, TellFailed, TellRequested,
    };
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

    fn tell_requested(
        tell_id: u64,
        target: &str,
        crash_restart: Option<Vec<u8>>,
    ) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Outbox(OutboxEvent::TellRequested(TellRequested {
            tell_id,
            crash_restart,
            target: target.to_owned(),
        }))
    }

    fn tell_acked(tell_id: u64) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Outbox(OutboxEvent::TellAcked(TellAcked { tell_id }))
    }

    fn tell_failed(tell_id: u64) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Outbox(OutboxEvent::TellFailed(TellFailed { tell_id }))
    }

    fn outbox_scheduled(at_unix_seconds: i64) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Outbox(OutboxEvent::Scheduled(OutboxScheduled { at_unix_seconds }))
    }

    fn ended() -> SagaPersisted<DomainEvt> {
        SagaPersisted::Outbox(OutboxEvent::Ended(Ended {}))
    }

    fn scheduled(
        name: &str,
        after: Duration,
        payload: &'static [u8],
        scheduled_at_unix_millis: i64,
    ) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Schedule(ScheduleEvent::Scheduled {
            token: token(name),
            after,
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

    fn fired(name: &str) -> SagaPersisted<DomainEvt> {
        SagaPersisted::Schedule(ScheduleEvent::Fired {
            token: token(name),
            fired_at_unix_millis: 0,
        })
    }

    fn dead_letter() -> SagaPersisted<DomainEvt> {
        SagaPersisted::DeadLetter(DeadLetterEvent {
            seq: 1,
            saga_id: saga_id(),
            failure: SagaFailure::HandleFailed {
                error: "injected".to_owned(),
            },
            occurred_at_unix_millis: 0,
            source: SourceContext::without_upstream(),
        })
    }

    #[test]
    fn tell_requested_becomes_a_pending_tell_with_target_and_payload() {
        let mut journal = JournalState::new(0);

        let handed_back = journal.fold(tell_requested(7, "target-stream", Some(vec![1, 2, 3])));

        assert!(
            handed_back.is_none(),
            "an outbox marker is folded into the journal, never handed back as a domain event"
        );
        let pending = journal
            .pending_tells
            .get(&7)
            .expect("TellRequested must register a pending tell keyed by its tell_id");
        assert_eq!(
            pending.target.as_ref().map(SagaId::as_str),
            Some("target-stream"),
            "a non-empty target must be preserved so replay can dead-letter the tell"
        );
        assert_eq!(
            pending.crash_restart.as_deref(),
            Some([1, 2, 3].as_slice()),
            "the crash-restart payload must be preserved verbatim for re-dispatch"
        );
    }

    #[test]
    fn tell_requested_with_empty_target_records_no_target() {
        let mut journal = JournalState::new(0);

        journal.fold(tell_requested(7, "", None));

        let pending = journal
            .pending_tells
            .get(&7)
            .expect("TellRequested must register a pending tell keyed by its tell_id");
        assert!(
            pending.target.is_none(),
            "an empty target means unknown, and must not become a target id of empty text"
        );
        assert!(
            pending.crash_restart.is_none(),
            "an absent crash-restart payload must stay absent"
        );
    }

    #[test]
    fn tell_acked_removes_the_pending_tell_without_recording_a_failure() {
        let mut journal = JournalState::new(0);
        journal.fold(tell_requested(7, "target-stream", None));

        journal.fold(tell_acked(7));

        assert!(
            journal.pending_tells.is_empty(),
            "an acked tell is settled and must not be re-dispatched"
        );
        assert!(
            journal.failed_tell_ids.is_empty(),
            "an acked tell succeeded and must never be reported as failed"
        );
    }

    #[test]
    fn tell_failed_removes_the_pending_tell_and_records_its_id() {
        let mut journal = JournalState::new(0);
        journal.fold(tell_requested(7, "target-stream", None));

        journal.fold(tell_failed(7));

        assert!(
            journal.pending_tells.is_empty(),
            "a failed tell is settled and must not be re-dispatched"
        );
        assert_eq!(
            journal.failed_tell_ids,
            vec![7],
            "a durable TellFailed must surface its tell_id to the saga"
        );
    }

    #[test]
    fn failed_tell_ids_keep_stream_order() {
        let mut journal = JournalState::new(0);
        for tell_id in [1, 2, 3] {
            journal.fold(tell_requested(tell_id, "target-stream", None));
        }

        journal.fold(tell_failed(3));
        journal.fold(tell_failed(1));
        journal.fold(tell_failed(2));

        assert_eq!(
            journal.failed_tell_ids,
            vec![3, 1, 2],
            "failed tell ids must be reported in the order the stream recorded them"
        );
    }

    #[test]
    fn ended_marker_marks_the_journal_ended() {
        let mut journal = JournalState::new(0);
        journal.fold(tell_requested(7, "target-stream", None));

        journal.fold(ended());

        assert!(
            journal.ended,
            "a durable Ended marker means the saga terminated in an earlier incarnation"
        );
        assert!(
            journal.pending_tells.contains_key(&7),
            "Ended records termination only; suppressing re-dispatch belongs to replay"
        );
    }

    #[test]
    fn outbox_scheduled_marker_leaves_the_journal_unchanged() {
        let mut journal = JournalState::new(0);

        journal.fold(outbox_scheduled(1_700_000_000));

        assert!(
            journal.active_schedules.is_empty(),
            "the outbox Scheduled marker is not a timer and must not register a schedule"
        );
        assert!(
            journal.pending_tells.is_empty(),
            "the outbox Scheduled marker must not register a pending tell"
        );
        assert!(
            journal.failed_tell_ids.is_empty(),
            "the outbox Scheduled marker must not record a failure"
        );
        assert!(
            !journal.ended,
            "the outbox Scheduled marker must not terminate the saga"
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
            journal.pending_tells.is_empty()
                && journal.failed_tell_ids.is_empty()
                && journal.active_schedules.is_empty()
                && !journal.ended,
            "a domain event carries no journal facts and must leave the projection untouched"
        );
    }

    #[test]
    fn dead_letter_is_not_folded_into_the_journal() {
        let mut journal = JournalState::new(0);

        let handed_back = journal.fold(dead_letter());

        assert!(
            handed_back.is_none(),
            "a dead letter is an observability side-channel, not a domain event to apply"
        );
        assert!(
            journal.pending_tells.is_empty()
                && journal.failed_tell_ids.is_empty()
                && journal.active_schedules.is_empty()
                && !journal.ended,
            "a dead letter must leave the projection untouched"
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
