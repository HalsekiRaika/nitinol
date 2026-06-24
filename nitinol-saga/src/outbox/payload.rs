use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendingEvent, EventType};

use crate::id::SagaId;
use crate::outbox::event_types::{
    OUTBOX_SCHEDULED, OUTBOX_TELL_ACKED, OUTBOX_TELL_FAILED, OUTBOX_TELL_REQUESTED,
};

pub(crate) fn encode_tell_id(tell_id: u64) -> Bytes {
    Bytes::from(tell_id.to_be_bytes().to_vec())
}

pub(crate) fn decode_tell_id(payload: &[u8]) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    let bytes: [u8; 8] = payload[..8].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

pub(crate) fn encode_tell_requested(tell_id: u64, crash_restart_payload: Option<&[u8]>) -> Bytes {
    let mut buf = tell_id.to_be_bytes().to_vec();
    if let Some(extra) = crash_restart_payload {
        buf.extend_from_slice(extra);
    }
    Bytes::from(buf)
}

pub(crate) fn decode_tell_requested(payload: &[u8]) -> Option<(u64, Option<Bytes>)> {
    if payload.len() < 8 {
        return None;
    }
    let bytes: [u8; 8] = payload[..8].try_into().ok()?;
    let tell_id = u64::from_be_bytes(bytes);
    let crash = if payload.len() > 8 {
        Some(Bytes::copy_from_slice(&payload[8..]))
    } else {
        None
    };
    Some((tell_id, crash))
}

pub(crate) fn encode_scheduled_at(at: jiff::Timestamp) -> Bytes {
    Bytes::from(at.as_second().to_be_bytes().to_vec())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TellOutcome {
    Acked,
    Failed,
}

impl TellOutcome {
    pub(crate) fn from_event_type(event_type: EventType) -> Option<Self> {
        if event_type == OUTBOX_TELL_ACKED {
            Some(TellOutcome::Acked)
        } else if event_type == OUTBOX_TELL_FAILED {
            Some(TellOutcome::Failed)
        } else {
            None
        }
    }
}

pub(crate) struct OutboxAppender;

impl OutboxAppender {
    pub(crate) fn build_tell_requested(
        sequence: u64,
        tell_id: u64,
        crash_restart_payload: Option<&[u8]>,
        occurred_at: jiff::Timestamp,
    ) -> AppendingEvent {
        AppendingEvent {
            sequence,
            event_type: OUTBOX_TELL_REQUESTED,
            payload: encode_tell_requested(tell_id, crash_restart_payload),
            occurred_at,
        }
    }

    pub(crate) fn build_scheduled(
        sequence: u64,
        at: jiff::Timestamp,
        occurred_at: jiff::Timestamp,
    ) -> AppendingEvent {
        AppendingEvent {
            sequence,
            event_type: OUTBOX_SCHEDULED,
            payload: encode_scheduled_at(at),
            occurred_at,
        }
    }

    pub(crate) async fn append_terminal(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        sequence: u64,
        outcome: TellOutcome,
        tell_id: u64,
    ) -> bool {
        let event_type = match outcome {
            TellOutcome::Acked => OUTBOX_TELL_ACKED,
            TellOutcome::Failed => OUTBOX_TELL_FAILED,
        };
        let event = AppendingEvent {
            sequence,
            event_type,
            payload: encode_tell_id(tell_id),
            occurred_at: jiff::Timestamp::now(),
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
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::EventStream;
    use nitinol_persistence::AppendOutcome;

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

    #[test]
    fn tell_outcome_from_event_type_returns_correct_variant_for_terminal_types() {
        use nitinol_persistence::EventType;
        assert_eq!(
            TellOutcome::from_event_type(EventType::from_str("nitinol.saga.outbox.tell_acked")),
            Some(TellOutcome::Acked),
            "tell_acked event_type must yield Some(Acked)"
        );
        assert_eq!(
            TellOutcome::from_event_type(EventType::from_str("nitinol.saga.outbox.tell_failed")),
            Some(TellOutcome::Failed),
            "tell_failed event_type must yield Some(Failed)"
        );
        assert_eq!(
            TellOutcome::from_event_type(EventType::from_str(
                "nitinol.saga.outbox.tell_requested"
            )),
            None,
            "tell_requested is not a terminal event_type"
        );
        assert_eq!(
            TellOutcome::from_event_type(EventType::from_str("user.SomeEvent")),
            None,
            "user event_type must yield None"
        );
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
