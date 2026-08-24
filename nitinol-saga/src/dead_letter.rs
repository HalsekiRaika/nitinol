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
//! This is a fresh, persisted, order-preserving implementation — distinct from
//! the in-memory, best-effort, system-wide `DeadLetterProcess` in
//! `nitinol-runtime`, which is neither imported nor reused here.
//!
//! [`DurableStream`]: nitinol_eventsource::DurableStream

mod disposition;
mod event;
mod policy;
mod queue;
mod subscriber;

use std::sync::Arc;

use nitinol_persistence::store::EventStore;

use crate::id::SagaId;

pub use self::event::{DeadLetterEvent, SagaFailure, SourceContext};
pub use self::policy::{EnqueueDecision, EnqueuePolicy};
pub use self::queue::{DeadLetterEntry, DeadLetterQuery, DeadLetterQueue, DeadLetterQueueError};

pub(crate) use self::disposition::{
    is_dead_letter_disposition_event_type, DeadLetterDispositionEvent,
};
// `Disposition` is an internal detail of the marker's codec in production, but
// the journal tests need it to build fold inputs without a store.
#[cfg(test)]
pub(crate) use self::disposition::Disposition;
pub(crate) use self::event::is_dead_letter_event_type;
pub(crate) use self::policy::default_enqueue_policy;
pub(crate) use self::subscriber::{make_dlq_child_spawn, DlqChildSpawn};

use self::event::{append_dead_letter, DeadLetterEvent as Event};

/// Outcome of [`enqueue_dead_letter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    /// The policy decided to ignore the failure.  No append was attempted; the
    /// caller may continue normally.
    Ignored,
    /// The policy decided to enqueue and the dead-letter was successfully
    /// appended to the saga's own stream.
    Enqueued,
    /// The policy decided to enqueue but the store append failed.  The saga's
    /// persisted DLQ contract is broken for this failure — the caller must not
    /// treat the upstream message as processed.
    AppendFailed,
}

/// Enqueue a saga `failure` as a dead letter on the saga's own stream, subject
/// to `policy`.
///
/// This is the single named operation every failure point routes through, so
/// the full set of DLQ writes is discoverable from one place.  On a successful
/// append `*sequence` advances by one; a failed append leaves it untouched so
/// no sequence number is skipped.
///
/// Callers **must** handle [`EnqueueOutcome::AppendFailed`]: when the policy
/// says `Enqueue` but the store write fails, the upstream message must not be
/// silently treated as processed — the persisted DLQ must stay authoritative.
pub(crate) async fn enqueue_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    policy: &dyn EnqueuePolicy,
    failure: SagaFailure,
    source: SourceContext,
) -> EnqueueOutcome {
    if matches!(policy.decide(&failure), EnqueueDecision::Ignore) {
        return EnqueueOutcome::Ignored;
    }
    let event = Event {
        seq: *sequence + 1,
        saga_id: saga_id.clone(),
        failure,
        occurred_at_unix_millis: jiff::Timestamp::now().as_millisecond(),
        source,
    };
    if append_dead_letter(store, saga_id, sequence, event).await {
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
    use crate::dead_letter::event::SourceContext;
    use crate::dead_letter::policy::{EnqueueAll, EnqueuePolicy};
    use crate::dead_letter::{DeadLetterQuery, DeadLetterQueue, EnqueueDecision, SagaFailure};
    use crate::id::SagaId;

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

        let outcome = enqueue_dead_letter(
            &store,
            &saga_id,
            &mut sequence,
            &IgnoreAllPolicy,
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SourceContext::without_upstream(),
        )
        .await;

        assert_eq!(outcome, EnqueueOutcome::Ignored);
        assert_eq!(sequence, 0, "sequence must not advance when policy ignores");
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

        let outcome = enqueue_dead_letter(
            &store,
            &saga_id,
            &mut sequence,
            &EnqueueAll,
            SagaFailure::HandleFailed {
                error: "e".to_owned(),
            },
            SourceContext::without_upstream(),
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
