//! Saga-owned Dead Letter Queue.
//!
//! Each saga failure kind, when it occurs, is enqueued — subject to the
//! per-saga [`EnqueuePolicy`] — as a [`DeadLetterEvent`] on the saga's **own**
//! EventStore stream (`SagaPersisted::DeadLetter`), mixed into the same
//! envelope as the outbox and schedule markers.  A [`DurableStream`] subscriber
//! catches up and receives the events.
//!
//! That subscriber is the **push** path — an observability signal, delivered
//! whether or not anyone has acted on it.  [`DeadLetterQueue`] is the **pull**
//! counterpart an operator recovers through: it lists the dead letters still
//! outstanding and settles them by appending a
//! `nitinol.saga.dead_letter_disposition` marker.  The two paths are
//! deliberately independent — a settled dead letter is dropped from the listing
//! and still delivered to subscribers, because a marker is a sibling family the
//! push path's dead-letter prefix does not select.
//!
//! Listing reads the store directly.  Settling does not: the marker goes onto
//! the saga's *own* stream, whose sequence a resident instance owns in memory,
//! so the write is routed through that stream's single arbiter — see
//! [`settle`] and [`SagaManagerProxy::dead_letter_queue`].
//!
//! This is a fresh, persisted, order-preserving implementation — distinct from
//! the in-memory, best-effort, system-wide `DeadLetterProcess` in
//! `nitinol-runtime`.  What the two share is an ownership pattern, not a
//! mechanism: neither type nor implementation is imported or reused here.
//!
//! [`DurableStream`]: nitinol_eventsource::DurableStream
//! [`SagaManagerProxy::dead_letter_queue`]: crate::SagaManagerProxy::dead_letter_queue

mod disposition;
mod event;
mod policy;
mod queue;
mod settle;
mod subscriber;

use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_persistence::store::EventStore;

use crate::consumed::{
    append_consumed_position, finalize_consumed_position, ConsumedPositionEvent, ConsumedPositions,
};
use crate::id::SagaId;

pub use self::event::{DeadLetterEvent, SagaFailure, SourceContext};
pub use self::policy::{EnqueueDecision, EnqueuePolicy};
pub use self::queue::{DeadLetterEntry, DeadLetterQuery, DeadLetterQueue, DeadLetterQueueError};

pub(crate) use self::disposition::DeadLetterDispositionEvent;
// `Disposition` is an internal detail of the marker's codec in production, but
// the journal and envelope tests need it to build markers without a store.
#[cfg(test)]
pub(crate) use self::disposition::Disposition;
pub(crate) use self::policy::default_enqueue_policy;
pub(crate) use self::settle::{
    DispositionArbiter, RecordDisposition, SettleDeadLetter, SettleError,
};
pub(crate) use self::subscriber::{make_dlq_child_spawn, DlqChildSpawn};

use self::event::DeadLetterEvent as Event;

/// Outcome of [`enqueue_dead_letter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    /// The policy decided to ignore the failure: no dead-letter row was
    /// written.  When `input_position` named one, its position-only append
    /// still landed, so the upstream record this call settles is durably
    /// consumed either way.
    Ignored,
    /// The policy decided to enqueue and the dead-letter was successfully
    /// appended to the saga's own stream.
    Enqueued,
    /// The store append failed — either the dead-letter row the policy asked
    /// for, or, when the policy ignored the failure, the position-only append
    /// that was standing in for it.  Either way the saga's persisted record of
    /// this input is not durable — the caller must not treat the upstream
    /// message as processed.
    AppendFailed,
}

/// Enqueue a saga `failure` as a dead letter on the saga's own stream, subject
/// to `policy` — plus the consumed position, when `input_position` names one
/// this call is settling, in the same atomic append.
///
/// This is the single named operation every failure point routes through, so
/// the full set of DLQ writes is discoverable from one place.  On a successful
/// append `*sequence` advances past every row written; a failed append leaves
/// it untouched so no sequence number is skipped.
///
/// `input_position` is `None` at call sites with no upstream input to settle
/// (timer firings, tell-outcome reports), and also at `PersistFailed` — the
/// batch that would have carried the position is exactly what never landed,
/// so recording it here would mark an undelivered decision as consumed.
/// Where it names one, the caller is settling that upstream record with an
/// `Ok` outcome (the `DirectPoller` will advance its cursor past it), so the
/// position must become durable in the same call whether or not the policy
/// also writes a dead letter: an `Ignore`d failure still gets a position-only
/// append, because "no dead letter" does not mean "not settled".
///
/// Callers **must** handle [`EnqueueOutcome::AppendFailed`]: the upstream
/// message must not be silently treated as processed when this call's
/// durable record of it — row, position, or both — did not land.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enqueue_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    policy: &dyn EnqueuePolicy,
    failure: SagaFailure,
    source: SourceContext,
    input_position: Option<&ConsumedPositionEvent>,
    consumed: &mut ConsumedPositions,
) -> EnqueueOutcome {
    if matches!(policy.decide(&failure), EnqueueDecision::Ignore) {
        let now = jiff::Timestamp::now();
        let mut appending = Vec::new();
        let mut next_seq = *sequence;
        let recorded_position =
            append_consumed_position(&mut appending, input_position, consumed, &mut next_seq, now);
        if appending.is_empty() {
            return EnqueueOutcome::Ignored;
        }
        return match store.append(saga_id.borrow(), appending).await {
            Ok(_) => {
                *sequence = next_seq;
                finalize_consumed_position(consumed, saga_id.as_str(), recorded_position);
                EnqueueOutcome::Ignored
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "saga consumed-position-only append failed (dead letter ignored by policy)"
                );
                EnqueueOutcome::AppendFailed
            }
        };
    }
    let event = Event {
        seq: *sequence + 1,
        saga_id: saga_id.clone(),
        failure,
        occurred_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
        source,
    };
    if event::append_dead_letter(store, saga_id, sequence, event, input_position, consumed).await {
        EnqueueOutcome::Enqueued
    } else {
        EnqueueOutcome::AppendFailed
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures_core::Stream;
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
    use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

    use super::{enqueue_dead_letter, EnqueueOutcome};
    use crate::consumed::{ConsumedPositionEvent, ConsumedPositions, PositionSource};
    use crate::dead_letter::event::SourceContext;
    use crate::dead_letter::policy::{EnqueueAll, EnqueuePolicy};
    use crate::dead_letter::{DeadLetterQuery, DeadLetterQueue, EnqueueDecision, SagaFailure};
    use crate::id::SagaId;
    use nitinol_persistence::AggregateId;

    /// The declared source the positions in these tests settle.  A position
    /// names one, and these tests are about whether it is written at all rather
    /// than about which source it belongs to, so one stand-in serves throughout.
    const SETTLED_SOURCE: nitinol_persistence::EventType = nitinol_persistence::EventType::new(
        nitinol_persistence::Family::new("dead_letter_test"),
        nitinol_persistence::TypeName::new("Observed"),
    );

    fn settled_source() -> PositionSource {
        PositionSource::of(SETTLED_SOURCE)
    }

    struct AlwaysFailStore;

    #[async_trait]
    impl EventStore for AlwaysFailStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            Err(AppendError::Backend("injected failure".into()))
        }

        async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let stream: Pin<
                Box<
                    dyn Stream<Item = Result<nitinol_persistence::LoadedEvent, LoadError>>
                        + Send
                        + '_,
                >,
            > = Box::pin(futures_util::stream::empty());
            Ok(stream)
        }
    }

    struct IgnoreAllPolicy;

    impl EnqueuePolicy for IgnoreAllPolicy {
        fn decide(&self, _failure: &SagaFailure) -> EnqueueDecision {
            EnqueueDecision::Ignore
        }
    }

    /// Regression test: when policy says `Ignore`, the outcome must
    /// be `Ignored` — not `AppendFailed`.  Callers rely on this to distinguish
    /// "intentionally skipped" from "write failure".
    #[tokio::test]
    async fn enqueue_outcome_is_ignored_when_policy_ignores_failure() {
        let store: Arc<dyn EventStore> = Arc::new(AlwaysFailStore);
        let saga_id = SagaId::new("saga-1");
        let mut sequence: u64 = 0;
        let mut consumed = ConsumedPositions::default();

        let outcome = enqueue_dead_letter(
            &store,
            &saga_id,
            &mut sequence,
            &IgnoreAllPolicy,
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SourceContext::without_upstream(),
            None,
            &mut consumed,
        )
        .await;

        assert_eq!(outcome, EnqueueOutcome::Ignored);
        assert_eq!(sequence, 0, "sequence must not advance when policy ignores");
    }

    /// Regression test: when policy says `Ignore` but `input_position` names
    /// an upstream record this call is settling, the position must still
    /// become durable — "no dead letter" is not "not settled", and the caller
    /// is about to return `Ok` either way.  Before this fix an ignored
    /// failure never appended anything, so a record settled this way was
    /// silently redeliverable after a restart.
    #[tokio::test]
    async fn enqueue_outcome_ignored_with_a_position_still_commits_it() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("saga-1");
        let mut sequence: u64 = 0;
        let mut consumed = ConsumedPositions::default();
        let position = ConsumedPositionEvent {
            source: AggregateId::new("upstream-order"),
            source_event_type: settled_source(),
            sequence: 7,
        };

        let outcome = enqueue_dead_letter(
            &store,
            &saga_id,
            &mut sequence,
            &IgnoreAllPolicy,
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SourceContext::without_upstream(),
            Some(&position),
            &mut consumed,
        )
        .await;

        assert_eq!(
            outcome,
            EnqueueOutcome::Ignored,
            "no dead letter is written, so the outcome is still Ignored"
        );
        assert_eq!(
            sequence, 1,
            "the position-only append must still advance the sequence"
        );
        assert!(
            consumed.covers("upstream-order", &settled_source(), 7),
            "the position must be recorded in memory, the same as a written \
             dead letter's would be"
        );
    }

    /// Regression test: when policy says `Enqueue` but the store
    /// append fails, the outcome must be `AppendFailed` so callers can stop the
    /// process and keep the persisted DLQ authoritative.  Before this fix the
    /// return value was discarded and the failure was silently lost.
    #[tokio::test]
    async fn enqueue_outcome_is_append_failed_when_store_rejects_write() {
        let store: Arc<dyn EventStore> = Arc::new(AlwaysFailStore);
        let saga_id = SagaId::new("saga-1");
        let mut sequence: u64 = 0;
        let mut consumed = ConsumedPositions::default();

        let outcome = enqueue_dead_letter(
            &store,
            &saga_id,
            &mut sequence,
            &EnqueueAll,
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SourceContext::without_upstream(),
            None,
            &mut consumed,
        )
        .await;

        assert_eq!(outcome, EnqueueOutcome::AppendFailed);
        assert_eq!(
            sequence, 0,
            "sequence must not advance when the append fails"
        );
    }

    /// The write path and the pull path have to agree about what a dead letter
    /// is.  The other DLQ pull tests seed the store directly, which cannot
    /// catch the two drifting apart — this one drives the loop end to end:
    /// [`enqueue_dead_letter`] writes, [`DeadLetterQueue::list`] reads back, and
    /// each settling arm removes exactly its own entry.
    #[tokio::test]
    async fn dead_letters_written_by_the_enqueue_path_are_listed_and_settled_by_the_queue() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("saga-1");
        let mut sequence: u64 = 0;
        let mut consumed = ConsumedPositions::default();

        for error in ["first", "second"] {
            let outcome = enqueue_dead_letter(
                &store,
                &saga_id,
                &mut sequence,
                &EnqueueAll,
                SagaFailure::HandleFailed {
                    error: error.to_owned(),
                },
                SourceContext::without_upstream(),
                None,
                &mut consumed,
            )
            .await;
            assert_eq!(outcome, EnqueueOutcome::Enqueued);
        }
        assert_eq!(sequence, 2, "each successful append advances the sequence");

        let queue = DeadLetterQueue::new(Arc::clone(&store), saga_id.clone());

        let listed = queue
            .list(DeadLetterQuery::default())
            .await
            .expect("listing the freshly enqueued dead letters");
        let seen: Vec<(u64, String)> = listed
            .iter()
            .map(|entry| match &entry.event.failure {
                SagaFailure::HandleFailed { error } => (entry.sequence, error.clone()),
                other => panic!("expected the HandleFailed the enqueue path wrote, got {other:?}"),
            })
            .collect();
        assert_eq!(
            seen,
            vec![(1, "first".to_owned()), (2, "second".to_owned())],
            "list must return what the enqueue path wrote, at the stream \
             sequences it wrote them to, with the recovery material intact"
        );

        queue.mark_processed(1).await.expect("marking seq 1");
        let after_processed = queue
            .list(DeadLetterQuery::default())
            .await
            .expect("listing after mark_processed");
        assert_eq!(
            after_processed
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![2],
            "mark_processed must drop only the dead letter it names"
        );

        queue.evict(2).await.expect("evicting seq 2");
        assert!(
            queue
                .list(DeadLetterQuery::default())
                .await
                .expect("listing after evict")
                .is_empty(),
            "evict must drop the last outstanding dead letter"
        );
    }
}
