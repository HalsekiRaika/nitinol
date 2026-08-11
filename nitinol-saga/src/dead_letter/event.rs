//! `DeadLetterEvent` — the `SystemEvent` persisted for each saga failure.
//!
//! Modelled one-for-one on the `ScheduleEvent` / `OutboxEvent` canonical
//! examples: a reserved type-level [`EventType`] (`nitinol.saga.dead_letter`,
//! variant `None`) whose per-arm `variant()` writes the failure kind on the
//! wire, with a prost `oneof` codec that owns its own serialization.

use std::borrow::Borrow;
use std::sync::Arc;

use bytes::Bytes;
use nitinol_eventsource::{appending_system_event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName, Variant};

use crate::id::SagaId;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.dead_letter.rs"));
}

use self::proto::dead_letter_marker::Failure as ProtoFailure;
use self::proto::{
    DeadLetterMarker, DecodeFailed, EndedSagaReceivedMessage, HandleFailed, PersistFailed,
    ScheduledFailed, SourceContext as ProtoSourceContext, TellFailed,
};

/// Reserved type-level identity shared by every dead-letter event.
pub(crate) const DEAD_LETTER_MARKER: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

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
    /// target aggregate's stream key; `message` is the tell's crash-restart
    /// payload when one was supplied, else empty.
    TellFailed { target: SagaId, message: Bytes },
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

    fn variant(&self) -> EventType {
        let variant = match &self.failure {
            SagaFailure::TellFailed { .. } => Variant::new("tell_failed"),
            SagaFailure::HandleFailed { .. } => Variant::new("handle_failed"),
            SagaFailure::PersistFailed { .. } => Variant::new("persist_failed"),
            SagaFailure::EndedSagaReceivedMessage { .. } => {
                Variant::new("ended_saga_received_message")
            }
            SagaFailure::DecodeFailed { .. } => Variant::new("decode_failed"),
            SagaFailure::ScheduledFailed { .. } => Variant::new("scheduled_failed"),
        };
        EventType::with_variant(
            DEAD_LETTER_MARKER.family(),
            DEAD_LETTER_MARKER.type_name(),
            variant,
        )
    }

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
                    target: SagaId::new(m.target),
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

/// Type-level classification: does `event_type` belong to the dead-letter
/// family (`nitinol.saga.dead_letter` and its per-arm variants)?
///
/// `nitinol.saga.dead_letter` is a sibling of `nitinol.saga.outbox` and
/// `nitinol.saga.schedule`, so the classifications never overlap.
pub(crate) fn is_dead_letter_event_type(event_type: EventType) -> bool {
    event_type
        .to_path()
        .is_within(&DEAD_LETTER_MARKER.to_path())
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
    let appending = appending_system_event(candidate, &event, now);
    if let Err(e) = store.append(saga_id.borrow(), vec![appending]).await {
        tracing::warn!(error = %e, "saga dead letter append failed");
        return false;
    }
    *sequence = candidate;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn dead_letter_reserves_a_single_type_level_event_type() {
        assert_eq!(DeadLetterEvent::EVENT_TYPE, DEAD_LETTER_MARKER);
        assert_eq!(DEAD_LETTER_MARKER.to_string(), "nitinol.saga.dead_letter");
        assert!(
            DEAD_LETTER_MARKER.variant().is_none(),
            "the reserved dead letter type is type-level (variant None)"
        );
    }

    #[test]
    fn each_failure_writes_its_per_arm_variant_on_the_wire() {
        let cases = [
            (
                SagaFailure::TellFailed {
                    target: SagaId::new("inventory-1"),
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
        ];
        for (failure, expected) in cases {
            let e = event(failure);
            assert_eq!(e.variant().variant(), Some(Variant::new(expected)));
            assert_eq!(e.variant().type_key(), DEAD_LETTER_MARKER.type_key());
        }
    }

    #[test]
    fn tell_failed_round_trips_through_the_enum_codec() {
        let e = event(SagaFailure::TellFailed {
            target: SagaId::new("inventory-42"),
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
                        assert_eq!(target, SagaId::new("inventory-42"));
                        assert_eq!(message, Bytes::from_static(b"payload"));
                    }
                    _ => panic!("expected decoded TellFailed"),
                }
            }
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    #[test]
    fn is_dead_letter_event_type_accepts_reserved_type_and_variants_but_not_siblings() {
        assert!(is_dead_letter_event_type(DEAD_LETTER_MARKER));
        assert!(is_dead_letter_event_type(EventType::with_variant(
            DEAD_LETTER_MARKER.family(),
            DEAD_LETTER_MARKER.type_name(),
            Variant::new("handle_failed"),
        )));
        assert!(!is_dead_letter_event_type(EventType::new(
            Family::new("nitinol.saga"),
            TypeName::new("outbox"),
        )));
        assert!(!is_dead_letter_event_type(EventType::new(
            Family::new("nitinol.saga"),
            TypeName::new("schedule"),
        )));
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
