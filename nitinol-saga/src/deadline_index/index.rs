//! What one index partition holds: the pending deadlines the schedule and
//! lifecycle facts it has read leave behind, ordered by when each is due.
//!
//! No IO: the partition reads the journal and hands each fact here, so the
//! folding rules are the same whichever store the facts came from.

use std::collections::{BTreeMap, HashMap};

use crate::id::SagaId;
use crate::scheduler::{ScheduleToken, TimerName};

/// Orders deadlines by due time, with the order a deadline was added in to
/// tell apart two that fall due at the same instant.
type DueKey = (jiff::Timestamp, u64);

/// The pending deadlines of the sagas one partition owns.
///
/// One deadline per `(saga, timer name)`, as on the saga's own stream: a
/// `scheduled` fact for a name already held replaces it, a `cancelled` or
/// `fired` fact removes it, and an `ended` fact removes every deadline of that
/// saga.
#[derive(Default)]
pub(crate) struct DeadlineIndex {
    by_due: BTreeMap<DueKey, ScheduleToken>,
    by_saga: HashMap<SagaId, HashMap<TimerName, DueKey>>,
    /// The position of the `ended` fact of each saga whose `scheduled` facts
    /// may still be read after it.
    ///
    /// Schedule and lifecycle facts are read by two queries, each in its own
    /// global order, so a saga's `ended` fact can be read before a `scheduled`
    /// fact that precedes it. Without this, that late fact would bring back a
    /// deadline of a saga that has ended.
    ended_at: HashMap<SagaId, u64>,
    next_serial: u64,
}

impl DeadlineIndex {
    /// The fact at global position `at` set `token`'s deadline to `due_at`.
    pub(crate) fn scheduled(&mut self, token: ScheduleToken, due_at: jiff::Timestamp, at: u64) {
        if self
            .ended_at
            .get(&token.saga_id)
            .is_some_and(|ended| at < *ended)
        {
            return;
        }
        self.remove(&token);

        let key = (due_at, self.next_serial);
        self.next_serial += 1;
        self.by_saga
            .entry(token.saga_id.clone())
            .or_default()
            .insert(token.name.clone(), key);
        self.by_due.insert(key, token);
    }

    /// `token`'s deadline is over: cancelled, or fired.
    pub(crate) fn remove(&mut self, token: &ScheduleToken) {
        let Some(timers) = self.by_saga.get_mut(&token.saga_id) else {
            return;
        };
        if let Some(key) = timers.remove(&token.name) {
            self.by_due.remove(&key);
        }
        if timers.is_empty() {
            self.by_saga.remove(&token.saga_id);
        }
    }

    /// `saga_id` ended, by the fact at global position `at`.
    pub(crate) fn ended(&mut self, saga_id: SagaId, at: u64) {
        if let Some(timers) = self.by_saga.remove(&saga_id) {
            for key in timers.into_values() {
                self.by_due.remove(&key);
            }
        }
        let ended = self.ended_at.entry(saga_id).or_insert(at);
        *ended = (*ended).max(at);
    }

    /// The schedule facts have been read up to `schedule_position`, so none
    /// below an `ended` fact at or under it is still to come.
    pub(crate) fn forget_ended_through(&mut self, schedule_position: u64) {
        self.ended_at.retain(|_, ended| *ended > schedule_position);
    }

    /// The deadlines due at or before `now`, earliest first.
    ///
    /// Reading them takes nothing out: a deadline stays until the journal says
    /// it is over.
    pub(crate) fn due_by(
        &self,
        now: jiff::Timestamp,
    ) -> impl Iterator<Item = (&ScheduleToken, jiff::Timestamp)> {
        self.by_due
            .range(..=(now, u64::MAX))
            .map(|((due_at, _), token)| (token, *due_at))
    }

    /// The earliest deadline still to come after `now`.
    ///
    /// The ones already due are not in question: they are asked for every
    /// turn until the journal settles them.
    pub(crate) fn next_after(&self, now: jiff::Timestamp) -> Option<jiff::Timestamp> {
        self.by_due
            .range((now, u64::MAX)..)
            .next()
            .map(|((due_at, _), _)| *due_at)
    }

    /// How many deadlines are held.
    pub(crate) fn len(&self) -> usize {
        self.by_due.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(saga: &str, name: &str) -> ScheduleToken {
        ScheduleToken::new(SagaId::new(saga), TimerName::new(name))
    }

    fn at(millis: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_millisecond(millis).expect("a test instant is in range")
    }

    fn due(index: &DeadlineIndex, now: i64) -> Vec<(ScheduleToken, i64)> {
        index
            .due_by(at(now))
            .map(|(token, due_at)| (token.clone(), due_at.as_millisecond()))
            .collect()
    }

    #[test]
    fn a_rescheduled_name_replaces_the_earlier_deadline() {
        let mut index = DeadlineIndex::default();
        index.scheduled(token("s", "t"), at(100), 1);
        index.scheduled(token("s", "t"), at(300), 2);

        assert_eq!(index.len(), 1);
        assert_eq!(due(&index, 1_000), vec![(token("s", "t"), 300)]);
    }

    #[test]
    fn cancelled_or_fired_removes_only_that_name() {
        let mut index = DeadlineIndex::default();
        index.scheduled(token("s", "a"), at(100), 1);
        index.scheduled(token("s", "b"), at(200), 2);

        index.remove(&token("s", "a"));
        index.remove(&token("s", "never-set"));

        assert_eq!(due(&index, 1_000), vec![(token("s", "b"), 200)]);
    }

    #[test]
    fn ended_removes_every_deadline_of_that_saga_only() {
        let mut index = DeadlineIndex::default();
        index.scheduled(token("s", "a"), at(100), 1);
        index.scheduled(token("s", "b"), at(200), 2);
        index.scheduled(token("other", "a"), at(300), 3);

        index.ended(SagaId::new("s"), 4);

        assert_eq!(due(&index, 1_000), vec![(token("other", "a"), 300)]);
    }

    #[test]
    fn a_scheduled_fact_read_after_the_ended_fact_it_precedes_stays_out() {
        let mut index = DeadlineIndex::default();
        index.ended(SagaId::new("s"), 7);

        index.scheduled(token("s", "t"), at(100), 6);

        assert_eq!(index.len(), 0);
    }

    #[test]
    fn the_ended_mark_is_forgotten_once_the_schedule_facts_have_passed_it() {
        let mut index = DeadlineIndex::default();
        index.ended(SagaId::new("s"), 7);

        index.forget_ended_through(6);
        index.scheduled(token("s", "t"), at(100), 6);
        assert_eq!(
            index.len(),
            0,
            "a fact below the ended mark is still held off"
        );

        index.forget_ended_through(7);
        assert!(index.ended_at.is_empty());
    }

    #[test]
    fn reading_the_due_deadlines_takes_none_out() {
        let mut index = DeadlineIndex::default();
        index.scheduled(token("s", "early"), at(100), 1);
        index.scheduled(token("s", "late"), at(500), 2);

        assert_eq!(due(&index, 100), vec![(token("s", "early"), 100)]);
        assert_eq!(due(&index, 100), vec![(token("s", "early"), 100)]);
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn the_next_deadline_is_the_earliest_one_still_to_come() {
        let mut index = DeadlineIndex::default();
        assert_eq!(index.next_after(at(0)), None);

        index.scheduled(token("s", "past"), at(100), 1);
        index.scheduled(token("s", "late"), at(900), 2);
        index.scheduled(token("s", "soon"), at(400), 3);

        assert_eq!(index.next_after(at(100)), Some(at(400)));
        assert_eq!(index.next_after(at(400)), Some(at(900)));
        assert_eq!(index.next_after(at(900)), None);
    }

    #[test]
    fn deadlines_due_at_the_same_instant_are_all_answered() {
        let mut index = DeadlineIndex::default();
        index.scheduled(token("a", "t"), at(100), 1);
        index.scheduled(token("b", "t"), at(100), 2);

        assert_eq!(
            due(&index, 100),
            vec![(token("a", "t"), 100), (token("b", "t"), 100)]
        );
    }
}
