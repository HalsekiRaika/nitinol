//! `DeadLetterEvent` — the `SystemEvent` persisted for each saga failure.
//!
//! Modelled one-for-one on the `ScheduleEvent` / `OutboxEvent` canonical
//! examples: one reserved [`EventType`] (`nitinol.saga.dead_letter`) every dead
//! letter is written down as, with a prost `oneof` codec that owns its own
//! serialization and carries which failure the record holds.

use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{reserved_event, AggregateId, EventType};

use crate::id::SagaId;
use crate::observability::report_system_event;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.dead_letter.rs"));
}

use self::proto::dead_letter_marker::Failure as ProtoFailure;
use self::proto::{
    DeadLetterMarker, DecodeFailed, EndedSagaReceivedMessage, HandleFailed, PersistFailed,
    ScheduledFailed, SourceContext as ProtoSourceContext, TellFailed, TellFailedHookFailed,
};

reserved_event! {
    /// The one identity every dead letter is written down as.
    ///
    /// What a binary linking this crate collects for the dead-letter record, so
    /// that a load of a saga's own stream restores its failures rather than
    /// failing on them.
    pub(crate) const DEAD_LETTER_MARKER {
        family = "nitinol.saga",
        type_name = "dead_letter",
        declared_by = DeadLetterEvent,
    }

    // The failures a dead letter records — the DLQ's whole arm set. A stored
    // row names the declaration, so which failure it is lives in the
    // payload's `oneof`; these are the names that `oneof` is reported under,
    // and the ones the DLQ documentation uses. `arm()` lands on `SagaFailure`,
    // not `DeadLetterEvent`: the failure detail is what has a wire shape, the
    // envelope around it does not.
    arm_type = SagaFailure;
    SagaFailure::TellFailed { .. } => TELL_FAILED_ARM = "tell_failed",
    SagaFailure::HandleFailed { .. } => HANDLE_FAILED_ARM = "handle_failed",
    SagaFailure::PersistFailed { .. } => PERSIST_FAILED_ARM = "persist_failed",
    SagaFailure::EndedSagaReceivedMessage { .. } => ENDED_SAGA_RECEIVED_MESSAGE_ARM = "ended_saga_received_message",
    SagaFailure::DecodeFailed { .. } => DECODE_FAILED_ARM = "decode_failed",
    SagaFailure::ScheduledFailed { .. } => SCHEDULED_FAILED_ARM = "scheduled_failed",
    SagaFailure::TellFailedHookFailed { .. } => TELL_FAILED_HOOK_FAILED_ARM = "tell_failed_hook_failed",
}

#[derive(Debug, thiserror::Error)]
#[error("dead letter payload carried no `oneof` failure")]
struct MissingFailure;

#[derive(Debug, thiserror::Error)]
#[error("dead letter payload carried no `source` context")]
struct MissingSourceContext;

#[derive(Debug, thiserror::Error)]
#[error("dead letter TellFailed.target must not be empty; it is a required field")]
struct EmptyTellTarget;

/// The specific saga failure a dead letter records.
#[derive(Clone, Debug)]
pub enum SagaFailure {
    /// A tell that exhausted its staged retry budget.  `target` is the
    /// target aggregate's stream key — an [`AggregateId`], since it names the
    /// aggregate that was told, not a saga; `message` is the tell's
    /// crash-restart payload when one was supplied, else empty.
    TellFailed { target: AggregateId, message: Bytes },
    /// `Saga::handle` returned `Self::Error`.
    HandleFailed { error: String },
    /// `SagaEffect::Persist` failed to write to the EventStore.
    PersistFailed { error: String },
    /// A message routed to a saga that had already ended (Draining).  `message`
    /// carries the raw payload when available, else empty.
    EndedSagaReceivedMessage { message: Bytes },
    /// A received message failed to deserialize.
    DecodeFailed { error: String },
    /// `Saga::on_scheduled` returned `Self::Error`.
    ScheduledFailed { error: String },
    /// `Saga::on_tell_failed` returned `Self::Error`, so the compensation the
    /// saga would have run never happened.  The tell failure that triggered the
    /// hook is recorded separately as its own [`SagaFailure::TellFailed`].
    TellFailedHookFailed { error: String },
}

/// Context captured at the moment of failure.
#[derive(Clone, Debug)]
pub struct SourceContext {
    /// The causing upstream message's aggregate id, or empty when the failure
    /// did not originate from an upstream event (timer / persist failures).
    pub aggregate_id: AggregateId,
    /// The causing upstream message's stream sequence, or zero when absent.
    pub upstream_sequence: u64,
}

impl SourceContext {
    /// Context for a failure with no originating upstream message (timer
    /// firings, tell-outcome reports, persist failures).
    pub fn without_upstream() -> Self {
        Self {
            aggregate_id: AggregateId::new(""),
            upstream_sequence: 0,
        }
    }
}

/// A framework-managed dead-letter event persisted on the saga's own stream.
#[derive(Clone, Debug)]
pub struct DeadLetterEvent {
    /// Stream sequence this dead letter occupies on the saga stream.
    pub seq: u64,
    pub saga_id: SagaId,
    pub failure: SagaFailure,
    pub occurred_at_unix_millis: i64,
    pub source: SourceContext,
}

impl SystemEvent for DeadLetterEvent {
    const EVENT_TYPE: EventType = DEAD_LETTER_MARKER;

    fn encode(&self) -> Bytes {
        let failure = match &self.failure {
            SagaFailure::TellFailed { target, message } => ProtoFailure::TellFailed(TellFailed {
                target: target.as_str().to_owned(),
                message: message.to_vec(),
            }),
            SagaFailure::HandleFailed { error } => ProtoFailure::HandleFailed(HandleFailed {
                error: error.clone(),
            }),
            SagaFailure::PersistFailed { error } => ProtoFailure::PersistFailed(PersistFailed {
                error: error.clone(),
            }),
            SagaFailure::EndedSagaReceivedMessage { message } => {
                ProtoFailure::EndedSagaReceivedMessage(EndedSagaReceivedMessage {
                    message: message.to_vec(),
                })
            }
            SagaFailure::DecodeFailed { error } => ProtoFailure::DecodeFailed(DecodeFailed {
                error: error.clone(),
            }),
            SagaFailure::ScheduledFailed { error } => {
                ProtoFailure::ScheduledFailed(ScheduledFailed {
                    error: error.clone(),
                })
            }
            SagaFailure::TellFailedHookFailed { error } => {
                ProtoFailure::TellFailedHookFailed(TellFailedHookFailed {
                    error: error.clone(),
                })
            }
        };
        let marker = DeadLetterMarker {
            seq: self.seq,
            saga_id: self.saga_id.as_str().to_owned(),
            occurred_at_unix_millis: self.occurred_at_unix_millis,
            source: Some(ProtoSourceContext {
                aggregate_id: self.source.aggregate_id.as_str().to_owned(),
                upstream_sequence: self.source.upstream_sequence,
            }),
            failure: Some(failure),
        };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <DeadLetterMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        let failure = match marker.failure {
            Some(ProtoFailure::TellFailed(m)) => {
                // `TellFailed.target` is a required field — an empty
                // string is never a valid target stream key.  Reject it here
                // so invalid payloads cannot be re-injected from the EventStore
                // or subscriber catchup path.
                if m.target.is_empty() {
                    return Err(SystemEventDecodeError::new(EmptyTellTarget));
                }
                SagaFailure::TellFailed {
                    target: AggregateId::new(m.target),
                    message: Bytes::from(m.message),
                }
            }
            Some(ProtoFailure::HandleFailed(m)) => SagaFailure::HandleFailed { error: m.error },
            Some(ProtoFailure::PersistFailed(m)) => SagaFailure::PersistFailed { error: m.error },
            Some(ProtoFailure::EndedSagaReceivedMessage(m)) => {
                SagaFailure::EndedSagaReceivedMessage {
                    message: Bytes::from(m.message),
                }
            }
            Some(ProtoFailure::DecodeFailed(m)) => SagaFailure::DecodeFailed { error: m.error },
            Some(ProtoFailure::ScheduledFailed(m)) => {
                SagaFailure::ScheduledFailed { error: m.error }
            }
            Some(ProtoFailure::TellFailedHookFailed(m)) => {
                SagaFailure::TellFailedHookFailed { error: m.error }
            }
            None => return Err(SystemEventDecodeError::new(MissingFailure)),
        };
        let source = marker
            .source
            .ok_or_else(|| SystemEventDecodeError::new(MissingSourceContext))?;
        Ok(DeadLetterEvent {
            seq: marker.seq,
            saga_id: SagaId::new(marker.saga_id),
            failure,
            occurred_at_unix_millis: marker.occurred_at_unix_millis,
            source: SourceContext {
                aggregate_id: AggregateId::new(source.aggregate_id),
                upstream_sequence: source.upstream_sequence,
            },
        })
    }
}

/// Is `event_type` the dead-letter record's own declaration?
///
/// `nitinol.saga.dead_letter` is a sibling of `nitinol.saga.outbox` and
/// `nitinol.saga.schedule`, and a declaration answers for its own path alone,
/// so the classifications never overlap.
pub(crate) fn is_dead_letter_event_type(event_type: EventType) -> bool {
    event_type == DEAD_LETTER_MARKER
}

/// Append a single dead-letter event at `*sequence + 1`, advancing `*sequence`
/// only on success.  Mirrors the terminal-append discipline used for outbox and
/// schedule markers: a failed store append leaves the caller's sequence
/// untouched so the next attempt does not skip numbers.
pub(crate) async fn append_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: &mut u64,
    event: DeadLetterEvent,
) -> bool {
    let candidate = *sequence + 1;
    let now = jiff::Timestamp::now();
    let appending = nitinol_eventsource::appending_system_event(candidate, &event, now);
    if let Err(e) = store.append(saga_id.borrow(), vec![appending]).await {
        tracing::warn!(error = %e, "saga dead letter append failed");
        return false;
    }
    *sequence = candidate;
    // A dead letter's position on the saga stream is its identity — it is what
    // an operator settles it by — so that is the context the record carries.
    report_system_event!(
        DeadLetterEvent::EVENT_TYPE,
        event.failure.arm(),
        saga_id.as_str(),
        sequence = candidate,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use nitinol_persistence::__private::reserved_family;
    use nitinol_persistence::TypeName;

    use crate::dead_letter::disposition::DEAD_LETTER_DISPOSITION_MARKER;

    fn source() -> SourceContext {
        SourceContext {
            aggregate_id: AggregateId::new("order-1"),
            upstream_sequence: 7,
        }
    }

    fn event(failure: SagaFailure) -> DeadLetterEvent {
        DeadLetterEvent {
            seq: 3,
            saga_id: SagaId::new("saga-1"),
            failure,
            occurred_at_unix_millis: 1_700_000_000_000,
            source: source(),
        }
    }

    #[test]
    fn dead_letter_reserves_a_single_event_type() {
        assert_eq!(DeadLetterEvent::EVENT_TYPE, DEAD_LETTER_MARKER);
        assert_eq!(DEAD_LETTER_MARKER.to_string(), "nitinol.saga.dead_letter");
    }

    /// The arm a dead letter reports is the name of the `oneof` its payload
    /// carries. The column no longer distinguishes them, so this is the whole
    /// of what an operator has to tell one failure from another.
    #[test]
    fn each_failure_reports_the_arm_its_payload_carries() {
        let cases = [
            (
                SagaFailure::TellFailed {
                    target: AggregateId::new("inventory-1"),
                    message: Bytes::from_static(b"m"),
                },
                "tell_failed",
            ),
            (
                SagaFailure::HandleFailed {
                    error: "e".to_owned(),
                },
                "handle_failed",
            ),
            (
                SagaFailure::PersistFailed {
                    error: "e".to_owned(),
                },
                "persist_failed",
            ),
            (
                SagaFailure::EndedSagaReceivedMessage {
                    message: Bytes::new(),
                },
                "ended_saga_received_message",
            ),
            (
                SagaFailure::DecodeFailed {
                    error: "e".to_owned(),
                },
                "decode_failed",
            ),
            (
                SagaFailure::ScheduledFailed {
                    error: "e".to_owned(),
                },
                "scheduled_failed",
            ),
            (
                SagaFailure::TellFailedHookFailed {
                    error: "e".to_owned(),
                },
                "tell_failed_hook_failed",
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(event(failure).failure.arm(), expected);
        }
    }

    #[test]
    fn tell_failed_round_trips_through_the_enum_codec() {
        let e = event(SagaFailure::TellFailed {
            target: AggregateId::new("inventory-42"),
            message: Bytes::from_static(b"payload"),
        });
        match DeadLetterEvent::decode(&e.encode()) {
            Ok(decoded) => {
                assert_eq!(decoded.seq, 3);
                assert_eq!(decoded.saga_id, SagaId::new("saga-1"));
                assert_eq!(decoded.occurred_at_unix_millis, 1_700_000_000_000);
                assert_eq!(decoded.source.aggregate_id, AggregateId::new("order-1"));
                assert_eq!(decoded.source.upstream_sequence, 7);
                match decoded.failure {
                    SagaFailure::TellFailed { target, message } => {
                        assert_eq!(target, AggregateId::new("inventory-42"));
                        assert_eq!(message, Bytes::from_static(b"payload"));
                    }
                    _ => panic!("expected decoded TellFailed"),
                }
            }
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    /// The hook rejection carries the saga's own error text, and it must come
    /// back as `TellFailedHookFailed` — not as the `TellFailed` that records
    /// the underlying tell failure.
    #[test]
    fn tell_failed_hook_failed_round_trips_through_the_enum_codec() {
        let e = event(SagaFailure::TellFailedHookFailed {
            error: "compensation refused".to_owned(),
        });
        match DeadLetterEvent::decode(&e.encode()) {
            Ok(decoded) => match decoded.failure {
                SagaFailure::TellFailedHookFailed { error } => {
                    assert_eq!(error, "compensation refused");
                }
                other => panic!("expected decoded TellFailedHookFailed, got {other:?}"),
            },
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    #[test]
    fn is_dead_letter_event_type_accepts_the_declaration_but_not_its_siblings() {
        assert!(is_dead_letter_event_type(DEAD_LETTER_MARKER));
        assert!(!is_dead_letter_event_type(EventType::new(
            reserved_family("nitinol.saga"),
            TypeName::new("outbox"),
        )));
        assert!(!is_dead_letter_event_type(EventType::new(
            reserved_family("nitinol.saga"),
            TypeName::new("schedule"),
        )));
    }

    /// `DeadLetterQueue::list` selects dead letters by the dead-letter path as a
    /// prefix, so a disposition marker sitting anywhere under that path would be
    /// loaded as a dead letter and handed to [`DeadLetterEvent::decode`], which
    /// only ever fails.  Separation is what keeps settling a dead letter from
    /// breaking the listing that surfaces the rest.
    #[test]
    fn the_disposition_marker_and_the_dead_letter_are_disjoint_siblings() {
        assert!(
            !DEAD_LETTER_DISPOSITION_MARKER
                .to_path()
                .is_within(&DEAD_LETTER_MARKER.to_path()),
            "the disposition marker must not fall under the dead-letter prefix; \
             `{DEAD_LETTER_DISPOSITION_MARKER}` is within `{DEAD_LETTER_MARKER}`"
        );
    }

    #[test]
    fn decode_reports_error_for_payload_missing_oneof_failure() {
        assert!(DeadLetterEvent::decode(&[]).is_err());
    }

    #[test]
    fn decode_reports_error_for_payload_missing_source_context() {
        // Build a well-formed TellFailed payload with `source` explicitly absent.
        // `source` is required — a missing field must be rejected, not silently
        // defaulted.
        let marker = proto::DeadLetterMarker {
            seq: 1,
            saga_id: "saga-1".to_owned(),
            occurred_at_unix_millis: 0,
            source: None,
            failure: Some(ProtoFailure::TellFailed(TellFailed {
                target: "inventory-1".to_owned(),
                message: vec![],
            })),
        };
        let payload = prost::Message::encode_to_vec(&marker);
        assert!(
            DeadLetterEvent::decode(&payload).is_err(),
            "decode must fail when `source` context is absent (required field)"
        );
    }

    /// Regression test: `DeadLetterEvent::decode()` must reject a
    /// `TellFailed` marker whose `target` field is empty.  An empty target is
    /// never a valid saga stream key; allowing it would let invalid
    /// `SagaFailure::TellFailed { target: "" }` re-enter from EventStore replay
    /// or subscriber catchup.
    #[test]
    fn decode_rejects_tell_failed_with_empty_target() {
        let marker = proto::DeadLetterMarker {
            seq: 1,
            saga_id: "saga-1".to_owned(),
            occurred_at_unix_millis: 0,
            source: Some(proto::SourceContext {
                aggregate_id: "order-1".to_owned(),
                upstream_sequence: 0,
            }),
            failure: Some(ProtoFailure::TellFailed(TellFailed {
                target: "".to_owned(), // empty — must be rejected
                message: vec![],
            })),
        };
        let payload = prost::Message::encode_to_vec(&marker);
        assert!(
            DeadLetterEvent::decode(&payload).is_err(),
            "decode must fail when TellFailed.target is empty (required field)"
        );
    }

    /// Positive case: a non-empty `target` round-trips correctly
    /// through decode (guards against overly strict validation).
    #[test]
    fn decode_accepts_tell_failed_with_non_empty_target() {
        let marker = proto::DeadLetterMarker {
            seq: 1,
            saga_id: "saga-1".to_owned(),
            occurred_at_unix_millis: 0,
            source: Some(proto::SourceContext {
                aggregate_id: "order-1".to_owned(),
                upstream_sequence: 0,
            }),
            failure: Some(ProtoFailure::TellFailed(TellFailed {
                target: "inventory-1".to_owned(),
                message: vec![],
            })),
        };
        let payload = prost::Message::encode_to_vec(&marker);
        assert!(
            DeadLetterEvent::decode(&payload).is_ok(),
            "decode must succeed when TellFailed.target is non-empty"
        );
    }
}
