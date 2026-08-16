//! Saga-owned Dead Letter Queue.
//!
//! Each saga failure kind, when it occurs, is enqueued — subject to the
//! per-saga [`EnqueuePolicy`] — as a [`DeadLetterEvent`] on the saga's **own**
//! EventStore stream (`SagaPersisted::DeadLetter`), mixed into the same
//! envelope as the outbox and schedule markers.  A [`DurableStream`] subscriber
//! catches up and receives the events.
//!
//! This is a fresh, persisted, order-preserving implementation — distinct from
//! the in-memory, best-effort, system-wide `DeadLetterProcess` in
//! `nitinol-runtime`, which is neither imported nor reused here.
//!
//! [`DurableStream`]: nitinol_eventsource::DurableStream

mod event;
mod policy;
mod subscriber;

use std::sync::Arc;

use nitinol_persistence::store::EventStore;

use crate::id::SagaId;

pub use self::event::{DeadLetterEvent, SagaFailure, SourceContext};
pub use self::policy::{EnqueueDecision, EnqueuePolicy};

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
    use nitinol_persistence::store::{EventStore, EventStream};
    use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};

    use super::{enqueue_dead_letter, EnqueueOutcome};
    use crate::dead_letter::event::SourceContext;
    use crate::dead_letter::policy::{EnqueueAll, EnqueuePolicy};
    use crate::dead_letter::{EnqueueDecision, SagaFailure};
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
}
