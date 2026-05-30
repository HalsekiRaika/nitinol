//! Per-tell retry executor.
//!
//! Spawned by the interpreter after a Persist batch with tells is durably
//! appended.  Owns the [`TellIntent`] for as long as the retry loop runs.
//! Re-invokes the typed side effect up to [`RetryPolicy::max_attempts`] times
//! with exponential backoff.  On success appends a `TellAcked` outbox marker;
//! on exhaustion appends a `TellFailed` marker.
//!
//! When the terminal marker is appended **successfully**, the executor also
//! removes the entry from the
//! [`crate::process::pending_intents::PendingIntents`] registry so that a
//! subsequent supervised restart does not re-dispatch an already-settled tell.
//! If `append_terminal` fails the registry entry is kept intact so that the
//! next supervised restart can still re-dispatch.

use std::sync::Arc;

use nitinol_persistence::store::EventStore;
use tokio::sync::Mutex;

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::{OutboxAppender, RetryPolicy, TerminalKind};
use crate::process::pending_intents::PendingIntents;

pub(crate) fn spawn_outbox_executor(
    intent: TellIntent,
    tell_id: u64,
    policy: RetryPolicy,
    saga_id: SagaId,
    store: Arc<dyn EventStore>,
    sequence: Arc<Mutex<u64>>,
    pending_intents: PendingIntents,
    failed_tell_ids: Arc<Mutex<Vec<u64>>>,
) {
    tokio::spawn(async move {
        let succeeded = run_attempts(&intent, &policy, tell_id).await;
        let kind = if succeeded {
            TerminalKind::Acked
        } else {
            TerminalKind::Failed
        };
        let appended =
            OutboxAppender::append_terminal(&store, &saga_id, &sequence, kind, tell_id).await;
        if appended {
            // Remove from the registry only when the terminal marker was durably
            // persisted.  If append failed, keep the entry so a subsequent
            // supervised restart can still re-dispatch this tell.
            pending_intents.take(tell_id).await;
            if !succeeded {
                // TellFailed was durably appended — surface this to the next
                // Saga::handle call via the shared failed-tell-ids accumulator.
                failed_tell_ids.lock().await.push(tell_id);
            }
        }
    });
}

async fn run_attempts(intent: &TellIntent, policy: &RetryPolicy, tell_id: u64) -> bool {
    for attempt in 1..=policy.max_attempts {
        if attempt > 1 {
            tokio::time::sleep(policy.backoff_before(attempt)).await;
        }
        match intent.side.execute_once().await {
            Ok(()) => return true,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    attempt,
                    tell_id,
                    "saga tell attempt failed; will retry if attempts remain"
                );
            }
        }
    }
    tracing::warn!(
        max_attempts = policy.max_attempts,
        tell_id,
        "saga tell exhausted retries; appending TellFailed"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures_core::Stream;
    use futures_util::stream;
    use nitinol_eventsource::test_helpers::MockAggregateProxy;
    use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event};
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, LoadQuery};

    use crate::outbox::RetryPolicy;
    use crate::process::pending_intents::PendingIntents;
    use crate::{SagaId, TellIntent};

    // -----------------------------------------------------------------------
    // Minimal domain types — only used to construct a TellIntent
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct OkCmd;

    #[derive(Clone)]
    struct DummyEvent;

    impl Event for DummyEvent {
        const EVENT_TYPE: EventType = EventType::from_str("executor_test.DummyEvent");
    }

    #[derive(Default)]
    struct DummyAggregate;

    impl Aggregate for DummyAggregate {
        type Event = DummyEvent;
        fn apply(&mut self, _: DummyEvent) {}
    }

    #[async_trait]
    impl Decider<OkCmd> for DummyAggregate {
        type Rejection = std::convert::Infallible;

        async fn decide(
            &self,
            _: OkCmd,
            _: &mut Context,
        ) -> Result<Effect<DummyEvent>, Self::Rejection> {
            Ok(Effect::persist(DummyEvent))
        }
    }

    // -----------------------------------------------------------------------
    // An EventStore that fails the first `append` call, then succeeds.
    // -----------------------------------------------------------------------

    struct FailOnceStore {
        has_failed: AtomicBool,
    }

    impl FailOnceStore {
        fn new() -> Self {
            Self {
                has_failed: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl EventStore for FailOnceStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            if !self.has_failed.swap(true, Ordering::SeqCst) {
                Err(AppendError::Backend("injected failure".into()))
            } else {
                Ok(AppendOutcome {
                    assigned_sequences: vec![],
                    stream_version: 0,
                })
            }
        }

        async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let s: Pin<Box<dyn Stream<Item = Result<nitinol_persistence::LoadedEvent, LoadError>> + Send + '_>> =
                Box::pin(stream::empty());
            Ok(s)
        }
    }

    // -----------------------------------------------------------------------
    // Regression test: AI-45-006
    // -----------------------------------------------------------------------

    /// When `append_terminal` fails (store returns an error), the
    /// `PendingIntents` registry entry must **not** be removed.  Removing it
    /// unconditionally would make a subsequent supervised restart unable to
    /// re-dispatch the tell, leaving the outbox in an orphaned state.
    #[tokio::test]
    async fn terminal_append_failure_leaves_pending_intent_intact() {
        let mock = MockAggregateProxy::<DummyAggregate>::new();

        // Pre-register an intent so we can verify it survives the failed append.
        let pending = PendingIntents::new();
        pending
            .register(
                1,
                TellIntent::new::<DummyAggregate, OkCmd, _>(mock.clone(), OkCmd),
            )
            .await;

        // Spawn an executor that will: succeed at the tell attempt, then fail
        // at `append_terminal` (FailOnceStore's first call).
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let sequence = Arc::new(Mutex::new(0u64));

        spawn_outbox_executor(
            TellIntent::new::<DummyAggregate, OkCmd, _>(mock, OkCmd),
            1,
            RetryPolicy::default(),
            SagaId::new("test-pending-intents-integrity"),
            store,
            sequence,
            pending.clone(),
            Arc::new(Mutex::new(Vec::new())),
        );

        // Wait for the spawned task to complete (no backoff since first attempt
        // succeeds, and the append failure is immediate).
        tokio::time::sleep(Duration::from_millis(200)).await;

        let entry_present = pending.0.lock().await.contains_key(&1);
        assert!(
            entry_present,
            "PendingIntents entry must remain when append_terminal fails; \
             a subsequent supervised restart must still be able to re-dispatch"
        );
    }
}
