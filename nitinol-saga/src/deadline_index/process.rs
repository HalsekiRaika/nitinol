//! One partition of a saga type's deadline index: the loop that reads the
//! schedule and lifecycle facts, and asks for the instances whose deadlines
//! have fallen due.

use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};

use nitinol_eventsource::SystemEvent;
use nitinol_persistence::error::LoadError;
use nitinol_persistence::store::Journal;
use nitinol_persistence::{is_within_reserved_namespace, EventType, LoadQuery, LoadedEvent};
use nitinol_runtime::error::HandlerError;
use nitinol_runtime::process::{Driver, Process, ProcessContext, ProcessProxy};

use crate::components::deadline::due_at;
use crate::deadline_index::index::DeadlineIndex;
use crate::delivery::partition_of;
use crate::id::SagaId;
use crate::lifecycle::LifecycleEvent;
use crate::process::manager::{ActivateInstance, SagaManagerProcess};
use crate::saga::Saga;
use crate::scheduler::ScheduleEvent;

/// One partition of the deadline index over the saga type `S`.
///
/// Holds nothing it could not read again: a partition rebuilt by supervision
/// starts from the beginning of the journal with an empty index.
pub(crate) struct SagaDeadlineIndexPartition<S: Saga> {
    /// The deadline message type `S` declares, which is how this saga type's
    /// schedule facts are told from every other saga type's.
    message_type: &'static str,
    partition: usize,
    partitions: NonZeroUsize,
    journal: Journal,
    manager: ProcessProxy<SagaManagerProcess<S>>,
    /// The last schedule fact read.
    ///
    /// Apart from `lifecycle_cursor` because the two fact types are read by two
    /// queries, each in its own order: one cursor across both would pass
    /// over whatever one type committed below where the other had reached.
    schedule_cursor: u64,
    /// The last lifecycle fact read.
    lifecycle_cursor: u64,
    index: DeadlineIndex,
    /// Whether this incarnation has read the journal through once.
    rebuilt: bool,
}

impl<S: Saga> SagaDeadlineIndexPartition<S> {
    pub(crate) fn new(
        message_type: &'static str,
        partition: usize,
        partitions: NonZeroUsize,
        journal: Journal,
        manager: ProcessProxy<SagaManagerProcess<S>>,
    ) -> Self {
        Self {
            message_type,
            partition,
            partitions,
            journal,
            manager,
            schedule_cursor: 0,
            lifecycle_cursor: 0,
            index: DeadlineIndex::default(),
            rebuilt: false,
        }
    }

    /// One turn of the loop: read what is new, then ask for what is due.
    async fn tick(&mut self) {
        if let Err(failure) = self.read_new_facts().await {
            // Nothing is written and the cursors stay where the read stopped,
            // so the next turn asks again from there; what is due waits at
            // most that long.
            tracing::warn!(
                message_type = self.message_type,
                partition = self.partition,
                error = %failure,
                "saga deadline index could not read the journal; retrying on the next tick",
            );
            return;
        }
        self.index.forget_ended_through(self.schedule_cursor);

        if !self.rebuilt {
            self.rebuilt = true;
            tracing::info!(
                message_type = self.message_type,
                partition = self.partition,
                count = self.index.len(),
                "saga deadline index rebuilt",
            );
        }

        self.request_due().await;
    }

    /// Read the schedule facts, then the lifecycle facts, above where this
    /// partition has reached in each.
    ///
    /// Schedule first: an `ended` fact read before a `scheduled` fact below it
    /// is held off by the index, while the other way round costs at most one
    /// request the instance's own replay answers with its `ended` fact.
    async fn read_new_facts(&mut self) -> Result<(), LoadError> {
        let journal = self.journal.clone();

        let mut facts = journal
            .load(Self::above(ScheduleEvent::EVENT_TYPE, self.schedule_cursor))
            .await?;
        while let Some(fact) = facts.next().await {
            let fact = fact?;
            if self.owns(&fact) {
                self.fold_schedule(&fact);
            }
            self.schedule_cursor = fact.global_sequence;
        }

        let mut facts = journal
            .load(Self::above(
                LifecycleEvent::EVENT_TYPE,
                self.lifecycle_cursor,
            ))
            .await?;
        while let Some(fact) = facts.next().await {
            let fact = fact?;
            if self.owns(&fact) {
                self.fold_lifecycle(&fact);
            }
            self.lifecycle_cursor = fact.global_sequence;
        }

        Ok(())
    }

    /// Every saga's facts of `event_type` above `cursor`.
    ///
    /// Named by the one event type rather than the family the framework's
    /// facts share: the family also holds a consumed position for every
    /// upstream record a saga ever consumed, and reading those would make a
    /// rebuild grow with the upstream's history rather than with the deadlines.
    fn above(event_type: EventType, cursor: u64) -> LoadQuery {
        LoadQuery::by_event_type(event_type).with_from_global(cursor + 1)
    }

    /// Whether `fact` is about a saga this partition owns — the same
    /// assignment the delivery host makes.
    fn owns(&self, fact: &LoadedEvent) -> bool {
        partition_of(&fact.stream_key, self.partitions) == self.partition
    }

    fn fold_schedule(&mut self, fact: &LoadedEvent) {
        match ScheduleEvent::decode(&fact.payload) {
            Ok(ScheduleEvent::Scheduled {
                token,
                after,
                message_type,
                scheduled_at_unix_millis,
                ..
            }) => {
                if message_type == self.message_type {
                    self.index.scheduled(
                        token,
                        due_at(scheduled_at_unix_millis, after),
                        fact.global_sequence,
                    );
                }
            }
            Ok(ScheduleEvent::Cancelled { token, .. } | ScheduleEvent::Fired { token, .. }) => {
                self.index.remove(&token);
            }
            Err(failure) => self.report_unreadable(fact, &failure),
        }
    }

    fn fold_lifecycle(&mut self, fact: &LoadedEvent) {
        match LifecycleEvent::decode(&fact.payload) {
            // A saga's id is its stream key, and one inside the reserved
            // namespace is no saga's: naming it as one would panic, and a
            // partition restarted over the same row would panic again.
            Ok(LifecycleEvent::Ended) if is_within_reserved_namespace(&fact.stream_key) => {
                self.report_unreadable(fact, &"the stream key is in the reserved namespace");
            }
            Ok(LifecycleEvent::Ended) => {
                self.index
                    .ended(SagaId::new(fact.stream_key.clone()), fact.global_sequence);
            }
            Err(failure) => self.report_unreadable(fact, &failure),
        }
    }

    /// Pass over a fact that cannot be read.
    ///
    /// The instance's own replay refuses the same row, so no road brings that
    /// deadline to `handle`, and standing at the row would stop every other
    /// deadline of the partition behind it.
    fn report_unreadable(&self, fact: &LoadedEvent, failure: &dyn std::fmt::Display) {
        tracing::error!(
            message_type = self.message_type,
            partition = self.partition,
            global_sequence = fact.global_sequence,
            stream_key = fact.stream_key.as_str(),
            error = %failure,
            "saga deadline index passed over a fact it could not read",
        );
    }

    /// How long to wait before the next turn: until the next poll, or until
    /// the earliest deadline still to come if that is sooner.
    fn wait(&self, poll: Duration) -> Duration {
        let now = jiff::Timestamp::now();
        match self.index.next_after(now) {
            Some(due_at) => wait_for(now, due_at, poll),
            None => poll,
        }
    }

    /// Ask the manager to bring back every instance whose deadline is due.
    ///
    /// The deadline stays in the index: only a `fired`, `cancelled` or `ended`
    /// fact takes it out, so an instance that was not brought back is asked
    /// for again on the next turn.
    async fn request_due(&self) {
        for (token, due_at) in self.index.due_by(jiff::Timestamp::now()) {
            tracing::info!(
                message_type = self.message_type,
                partition = self.partition,
                saga_id = token.saga_id.as_str(),
                timer = token.name.as_str(),
                due_at_unix_millis = due_at.as_millisecond(),
                "saga deadline index requested activation",
            );
            let request = ActivateInstance {
                saga_id: token.saga_id.clone(),
            };
            if let Err(failure) = self.manager.tell(request).await {
                tracing::warn!(
                    message_type = self.message_type,
                    partition = self.partition,
                    saga_id = token.saga_id.as_str(),
                    error = %failure,
                    "saga deadline index could not reach the manager; asking again on the next tick",
                );
            }
        }
    }
}

impl<S: Saga> Process for SagaDeadlineIndexPartition<S> {}

/// The wait until `due_at`, no longer than `poll`.
fn wait_for(now: jiff::Timestamp, due_at: jiff::Timestamp, poll: Duration) -> Duration {
    Duration::try_from(due_at.duration_since(now)).map_or(poll, |until_due| until_due.min(poll))
}

/// Drives an index partition: a turn every poll interval, and one at the
/// earliest deadline still to come when that falls sooner.
///
/// Read first, then sleep, as a delivery partition does: the first turn comes
/// as soon as the partition starts, so a deadline already due is asked for
/// without waiting an interval. The poll interval stays the longest a later
/// turn waits, so the journal is read for new facts and a deadline already
/// asked for is asked for again at that cadence. The driver lives as long as
/// its process: a supervised restart rebuilds the partition but not this, so
/// a rebuilt partition waits before its first read.
pub(crate) struct IndexDriver<S: Saga> {
    poll: Duration,
    /// How long `next` sleeps once the first turn has been handed out; the
    /// poll interval until a turn has read the deadlines.
    wait: Duration,
    has_ticked: bool,
    _saga: PhantomData<fn() -> S>,
}

impl<S: Saga> IndexDriver<S> {
    pub(crate) fn new(poll: Duration) -> Self {
        Self {
            poll,
            wait: poll,
            has_ticked: false,
            _saga: PhantomData,
        }
    }
}

impl<S: Saga> Driver<SagaDeadlineIndexPartition<S>> for IndexDriver<S> {
    type Event = ();

    async fn next(&mut self) -> Option<Self::Event> {
        // The flag is read and set inside the future rather than when it is
        // made: the lifecycle loop drops a `next` future it never polled when
        // a system signal is waiting, and a flag set at creation would lose
        // the first turn to that.
        if self.has_ticked {
            tokio::time::sleep(self.wait).await;
        }
        self.has_ticked = true;
        Some(())
    }

    async fn apply(
        &mut self,
        state: &mut SagaDeadlineIndexPartition<S>,
        _ctx: &mut ProcessContext<SagaDeadlineIndexPartition<S>>,
        _ev: (),
    ) -> Result<(), HandlerError> {
        // A panic in the journal a partition was handed is reported as a
        // handler failure, so supervision rebuilds the partition — which reads
        // the facts again from the start.
        match AssertUnwindSafe(state.tick()).catch_unwind().await {
            Ok(()) => {
                self.wait = state.wait(self.poll);
                Ok(())
            }
            Err(_panic) => {
                tracing::warn!(
                    message_type = state.message_type,
                    partition = state.partition,
                    "a saga deadline index partition panicked; supervision will restart it",
                );
                Err(HandlerError)
            }
        }
    }

    /// A partition is driven by its own cadence and deadlines, so there is no
    /// idleness for a timeout to read.
    fn supports_idle_timeout(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_millisecond(millis).expect("a test instant is in range")
    }

    #[test]
    fn a_deadline_before_the_next_poll_is_waited_for() {
        let wait = wait_for(at(1_000), at(1_300), Duration::from_millis(1_000));

        assert_eq!(wait, Duration::from_millis(300));
    }

    #[test]
    fn a_deadline_after_the_next_poll_waits_for_the_poll() {
        let wait = wait_for(at(1_000), at(9_000), Duration::from_millis(250));

        assert_eq!(wait, Duration::from_millis(250));
    }

    #[test]
    fn a_deadline_that_has_already_passed_waits_no_longer_than_the_poll() {
        let wait = wait_for(at(1_000), at(900), Duration::from_millis(250));

        assert_eq!(wait, Duration::from_millis(250));
    }
}
