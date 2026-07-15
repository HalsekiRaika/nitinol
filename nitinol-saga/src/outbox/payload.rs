use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_eventsource::appending_system_event;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AppendingEvent;

use crate::id::SagaId;
use crate::outbox::message::{Ended, OutboxEvent, TellAcked, TellFailed, TellRequested};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TellOutcome {
    Acked,
    Failed,
}

pub(crate) struct OutboxAppender;

impl OutboxAppender {
    pub(crate) fn build_tell_requested(
        sequence: u64,
        tell_id: u64,
        crash_restart_payload: Option<&[u8]>,
        target: &str,
        occurred_at: jiff::Timestamp,
    ) -> AppendingEvent {
        let message = OutboxEvent::TellRequested(TellRequested {
            tell_id,
            crash_restart: crash_restart_payload
                .filter(|b| !b.is_empty())
                .map(<[u8]>::to_vec),
            target: target.to_owned(),
        });
        appending_system_event(sequence, &message, occurred_at)
    }

    /// Append the durable `Ended` terminal marker at `sequence` on the saga's
    /// own stream.  Written when `SagaEffect::End` is interpreted so a later
    /// spawn can detect termination and refuse to revive the saga (D-14).
    ///
    /// Returns `false` (and does not advance any caller-held sequence) when the
    /// store rejects the append, mirroring [`OutboxAppender::append_terminal`].
    pub(crate) async fn append_ended(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        sequence: u64,
    ) -> bool {
        let now = jiff::Timestamp::now();
        let event = appending_system_event(sequence, &OutboxEvent::Ended(Ended {}), now);
        if let Err(e) = store.append(saga_id.borrow(), vec![event]).await {
            tracing::warn!(error = %e, "saga outbox ended marker append failed");
            return false;
        }
        true
    }

    pub(crate) async fn append_terminal(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        sequence: u64,
        outcome: TellOutcome,
        tell_id: u64,
    ) -> bool {
        let now = jiff::Timestamp::now();
        let event = match outcome {
            TellOutcome::Acked => appending_system_event(
                sequence,
                &OutboxEvent::TellAcked(TellAcked { tell_id }),
                now,
            ),
            TellOutcome::Failed => appending_system_event(
                sequence,
                &OutboxEvent::TellFailed(TellFailed { tell_id }),
                now,
            ),
        };
        if let Err(e) = store.append(saga_id.borrow(), vec![event]).await {
            tracing::warn!(error = %e, ?outcome, "saga outbox terminal marker append failed");
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use futures_core::Stream;
    use nitinol_eventsource::SystemEvent;
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::AppendOutcome;

    #[test]
    fn build_tell_requested_normalizes_empty_crash_restart_bytes_to_none() {
        let event = OutboxAppender::build_tell_requested(
            1,
            42,
            Some(&[]),
            "",
            jiff::Timestamp::UNIX_EPOCH,
        );
        assert!(
            crate::outbox::is_outbox_event_type(event.event_type),
            "build_tell_requested must write an outbox-prefixed event type"
        );
        match OutboxEvent::decode(&event.payload) {
            Ok(OutboxEvent::TellRequested(m)) => {
                assert!(
                    m.crash_restart.is_none(),
                    "empty crash_restart bytes must be normalized to None, got {:?}",
                    m.crash_restart
                );
            }
            _ => panic!("expected decoded TellRequested from the built outbox payload"),
        }
    }

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

        async fn load(
            &self,
            _query: nitinol_persistence::LoadQuery,
        ) -> Result<EventStream<'_>, LoadError> {
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

    #[tokio::test]
    async fn append_terminal_returns_false_on_store_failure_and_true_on_success() {
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("append-terminal-contract");

        let ok = OutboxAppender::append_terminal(&store, &saga_id, 1, TellOutcome::Acked, 1).await;
        assert!(
            !ok,
            "append_terminal must return false when EventStore::append fails"
        );

        let ok = OutboxAppender::append_terminal(&store, &saga_id, 2, TellOutcome::Acked, 1).await;
        assert!(
            ok,
            "append_terminal must return true when EventStore::append succeeds"
        );
    }
}
