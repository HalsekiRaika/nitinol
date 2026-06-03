use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AppendingEvent, EventType};

use crate::id::SagaId;
use crate::outbox::event_types::{
    OUTBOX_PREFIX, OUTBOX_SCHEDULED, OUTBOX_TELL_ACKED, OUTBOX_TELL_FAILED, OUTBOX_TELL_REQUESTED,
};

/// Encode an 8-byte big-endian payload — used for `TellAcked` / `TellFailed`
/// / `Scheduled` markers, and as the fixed header of `TellRequested`.
pub(crate) fn encode_tell_id(tell_id: u64) -> Bytes {
    Bytes::from(tell_id.to_be_bytes().to_vec())
}

/// Decode the 8-byte big-endian `tell_id` from the **first** 8 bytes of a
/// payload.  Accepts payloads of any length ≥ 8 so that `TellRequested`
/// markers with an optional crash-restart suffix (see
/// [`encode_tell_requested`]) are still decoded correctly.  Returns `None`
/// when the payload is shorter than 8 bytes.
pub(crate) fn decode_tell_id(payload: &[u8]) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    let bytes: [u8; 8] = payload[..8].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// Encode a `TellRequested` payload: the 8-byte big-endian `tell_id` followed
/// by the optional crash-restart bytes.
///
/// Layout: `[tell_id (8 bytes)] [crash_restart_payload (0 or more bytes)]`
///
/// When `crash_restart_payload` is `None` the payload is exactly 8 bytes,
/// keeping backward compatibility with streams written before crash-restart
/// support was added.
pub(crate) fn encode_tell_requested(tell_id: u64, crash_restart_payload: Option<&[u8]>) -> Bytes {
    let mut buf = tell_id.to_be_bytes().to_vec();
    if let Some(extra) = crash_restart_payload {
        buf.extend_from_slice(extra);
    }
    Bytes::from(buf)
}

/// Decode a `TellRequested` payload.
///
/// Returns `(tell_id, crash_restart_payload)`:
/// - `payload[0..8]` → `tell_id`
/// - `payload[8..]`  → crash-restart bytes (`None` when the slice is empty,
///   i.e. the marker was written without crash-restart support).
///
/// Returns `None` when the payload is shorter than 8 bytes.
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

/// Encode a scheduled timestamp into an 8-byte big-endian seconds payload.
/// Keeps the outbox marker format uniformly 8 bytes wide.
pub(crate) fn encode_scheduled_at(at: jiff::Timestamp) -> Bytes {
    Bytes::from(at.as_second().to_be_bytes().to_vec())
}

/// Classification of an outbox event during replay.
///
/// Non-outbox event types are classified as `User`; the replay loop then
/// hands them to the user codec.  The discriminator is the event's
/// `event_type` string — payload is never inspected at classification time.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OutboxClassification {
    User,
    TellRequested,
    TellAcked,
    TellFailed,
    Scheduled,
}

pub(crate) struct OutboxClassifier;

impl OutboxClassifier {
    pub(crate) fn classify(event_type: EventType) -> OutboxClassification {
        let s = event_type.as_str();
        if !s.starts_with(OUTBOX_PREFIX) {
            return OutboxClassification::User;
        }
        if s == OUTBOX_TELL_REQUESTED.as_str() {
            OutboxClassification::TellRequested
        } else if s == OUTBOX_TELL_ACKED.as_str() {
            OutboxClassification::TellAcked
        } else if s == OUTBOX_TELL_FAILED.as_str() {
            OutboxClassification::TellFailed
        } else if s == OUTBOX_SCHEDULED.as_str() {
            OutboxClassification::Scheduled
        } else {
            // Reserved-prefix event type that the framework does not know.
            // Treat as `User` so the codec fails fast — telling us a future
            // marker leaked into a build that does not understand it.
            OutboxClassification::User
        }
    }
}

/// Single source of truth for appending an outbox marker.  Used by:
/// - the interpreter's `persist_batch` (TellRequested / Scheduled, as part
///   of a larger atomic batch — see `OutboxAppender::build_tell_requested`
///   / `build_scheduled`)
/// - the retry executor's terminal marker append (TellAcked / TellFailed)
pub(crate) struct OutboxAppender;

impl OutboxAppender {
    /// Build a `TellRequested` outbox marker.
    ///
    /// `crash_restart_payload` is appended after the 8-byte `tell_id` so that
    /// the saga's [`crate::SagaProps::with_crash_restart_factory`] can
    /// reconstruct the [`crate::TellIntent`] after a full OS-process crash.
    /// Pass `None` when crash-restart re-dispatch is not needed.
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

    /// Append a `TellAcked` or `TellFailed` marker at the given pre-claimed
    /// sequence number.
    ///
    /// The caller (outbox retry executor) sends `AppendTerminalAndClaim` to the
    /// saga via `self_proxy.ask`, which atomically appends the terminal marker
    /// and advances the sequence cursor inside the saga's single-threaded loop.
    ///
    /// Returns `true` when the marker was durably appended, `false` on failure.
    /// Callers **must** check the return value before notifying the saga
    /// process to remove the matching `pending_intents` entry — removing it on
    /// failure would make a subsequent supervised restart unable to re-dispatch
    /// the tell.
    pub(crate) async fn append_terminal(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
        sequence: u64,
        kind: TerminalKind,
        tell_id: u64,
    ) -> bool {
        let event_type = match kind {
            TerminalKind::Acked => OUTBOX_TELL_ACKED,
            TerminalKind::Failed => OUTBOX_TELL_FAILED,
        };
        let event = AppendingEvent {
            sequence,
            event_type,
            payload: encode_tell_id(tell_id),
            occurred_at: jiff::Timestamp::now(),
        };
        if let Err(e) = store.append(saga_id.borrow(), vec![event]).await {
            tracing::warn!(error = %e, ?kind, "saga outbox terminal marker append failed");
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalKind {
    Acked,
    Failed,
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

    /// An `EventStore` that fails the first `append` call, then succeeds.
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
        // The caller claims the sequence number before invoking
        // `append_terminal`. This test exercises the bare contract: the function
        // must surface a true / false outcome that the caller uses to decide
        // whether to commit the pending-intent removal in the saga state.
        let store: Arc<dyn EventStore> = Arc::new(FailOnceStore::new());
        let saga_id = SagaId::new("append-terminal-contract");

        let ok = OutboxAppender::append_terminal(&store, &saga_id, 1, TerminalKind::Acked, 1).await;
        assert!(
            !ok,
            "append_terminal must return false when EventStore::append fails"
        );

        let ok = OutboxAppender::append_terminal(&store, &saga_id, 2, TerminalKind::Acked, 1).await;
        assert!(
            ok,
            "append_terminal must return true when EventStore::append succeeds"
        );
    }
}
