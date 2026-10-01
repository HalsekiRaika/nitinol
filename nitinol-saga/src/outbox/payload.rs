use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_eventsource::SystemEvent;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AppendingEvent;

use crate::consumed::{
    append_consumed_position, finalize_consumed_position, ConsumedPositionEvent, ConsumedPositions,
};
use crate::id::SagaId;
use crate::observability::report_system_event;
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
        message.appending(sequence, occurred_at)
    }

    /// Append the durable `Ended` terminal marker — plus the consumed
    /// position, when `input_position` names one this append is closing out —
    /// at `*sequence + 1` on the saga's own stream, advancing `*sequence` past
    /// every row only on success.  Written when `SagaEffect::End` is
    /// interpreted so a later spawn can detect termination and refuse to
    /// revive the saga.
    ///
    /// `input_position` is `None` at call sites with no upstream input to
    /// consume, and also whenever `End` is not the effect closing out its
    /// input — the interpreter's `Sequence` handling only ever hands the
    /// position to that one.
    ///
    /// Returns `false` (and does not advance `*sequence`) when the store
    /// rejects the append, mirroring [`OutboxAppender::append_terminal`].
    pub(crate) async fn append_ended(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        sequence: &mut u64,
        input_position: Option<&ConsumedPositionEvent>,
        consumed: &mut ConsumedPositions,
    ) -> bool {
        let now = jiff::Timestamp::now();
        let candidate = *sequence + 1;
        let marker = OutboxEvent::Ended(Ended {});
        let mut appending = vec![marker.appending(candidate, now)];
        let mut next_seq = candidate;
        let recorded_position =
            append_consumed_position(&mut appending, input_position, consumed, &mut next_seq, now);

        if let Err(e) = store.append(saga_id.borrow(), appending).await {
            tracing::warn!(error = %e, "saga outbox ended marker append failed");
            return false;
        }
        *sequence = next_seq;
        report_system_event!(
            OutboxEvent::EVENT_TYPE,
            marker.arm(),
            saga_id.as_str(),
            sequence = candidate,
        );
        finalize_consumed_position(consumed, saga_id.as_str(), recorded_position);
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
        let marker = match outcome {
            TellOutcome::Acked => OutboxEvent::TellAcked(TellAcked { tell_id }),
            TellOutcome::Failed => OutboxEvent::TellFailed(TellFailed { tell_id }),
        };
        let event = marker.appending(sequence, now);
        if let Err(e) = store.append(saga_id.borrow(), vec![event]).await {
            tracing::warn!(error = %e, ?outcome, "saga outbox terminal marker append failed");
            return false;
        }
        report_system_event!(
            OutboxEvent::EVENT_TYPE,
            marker.arm(),
            saga_id.as_str(),
            tell_id = tell_id,
        );
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
    use nitinol_eventsource::MatchesEventType;
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::AppendOutcome;

    #[test]
    fn build_tell_requested_normalizes_empty_crash_restart_bytes_to_none() {
        let event =
            OutboxAppender::build_tell_requested(1, 42, Some(&[]), "", jiff::Timestamp::UNIX_EPOCH);
        assert!(
            event.event_type.matches::<OutboxEvent>(),
            "build_tell_requested must write the outbox declaration's event type"
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
