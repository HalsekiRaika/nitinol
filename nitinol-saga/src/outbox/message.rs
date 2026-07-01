use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::EventType;

use crate::outbox::event_types::OUTBOX_MARKER;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.outbox.rs"));
}

pub(crate) use self::proto::{OutboxMarker, Scheduled, TellAcked, TellFailed, TellRequested};
use self::proto::outbox_marker::Kind;

#[derive(Debug, thiserror::Error)]
#[error("outbox marker payload carried no oneof kind")]
struct MissingOutboxKind;

pub(crate) enum OutboxMessage {
    TellRequested(TellRequested),
    TellAcked(TellAcked),
    TellFailed(TellFailed),
    Scheduled(Scheduled),
}

impl SystemEvent for OutboxMessage {
    const EVENT_TYPE: EventType = OUTBOX_MARKER;

    fn encode(&self) -> Bytes {
        let kind = match self {
            OutboxMessage::TellRequested(m) => Kind::TellRequested(m.clone()),
            OutboxMessage::TellAcked(m) => Kind::TellAcked(*m),
            OutboxMessage::TellFailed(m) => Kind::TellFailed(*m),
            OutboxMessage::Scheduled(m) => Kind::Scheduled(*m),
        };
        let marker = OutboxMarker { kind: Some(kind) };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker =
            <OutboxMarker as prost::Message>::decode(payload).map_err(SystemEventDecodeError::new)?;
        match marker.kind {
            Some(Kind::TellRequested(m)) => Ok(OutboxMessage::TellRequested(m)),
            Some(Kind::TellAcked(m)) => Ok(OutboxMessage::TellAcked(m)),
            Some(Kind::TellFailed(m)) => Ok(OutboxMessage::TellFailed(m)),
            Some(Kind::Scheduled(m)) => Ok(OutboxMessage::Scheduled(m)),
            None => Err(SystemEventDecodeError::new(MissingOutboxKind)),
        }
    }
}

impl OutboxMessage {
    pub(crate) fn classify(
        event_type: EventType,
        payload: &[u8],
    ) -> Option<Result<OutboxMessage, SystemEventDecodeError>> {
        if event_type.type_key() == OUTBOX_MARKER.type_key() {
            Some(OutboxMessage::decode(payload))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::event_types::OUTBOX_MARKER;
    use nitinol_persistence::{Family, TypeName, Variant};

    fn tell_requested(tell_id: u64, crash_restart: Option<Vec<u8>>) -> OutboxMessage {
        OutboxMessage::TellRequested(TellRequested {
            tell_id,
            crash_restart,
        })
    }

    #[test]
    fn outbox_message_reserves_a_single_event_type() {
        assert_eq!(OutboxMessage::EVENT_TYPE, OUTBOX_MARKER);
        assert_eq!(OUTBOX_MARKER.to_string(), "nitinol.saga.outbox");
        assert!(
            OUTBOX_MARKER.variant().is_none(),
            "the reserved outbox type is type-level (variant None)"
        );
    }

    #[test]
    fn every_marker_writes_the_same_wire_event_type() {
        let markers = [
            tell_requested(1, None),
            OutboxMessage::TellAcked(TellAcked { tell_id: 1 }),
            OutboxMessage::TellFailed(TellFailed { tell_id: 1 }),
            OutboxMessage::Scheduled(Scheduled { at_unix_seconds: 0 }),
        ];
        for m in &markers {
            assert_eq!(
                m.variant(),
                OUTBOX_MARKER,
                "no marker may override variant(); all share one wire event_type"
            );
        }
    }

    #[test]
    fn classify_round_trips_each_marker_through_the_enum_codec() {
        let requested = tell_requested(7, Some(vec![1, 2, 3]));
        match OutboxMessage::classify(OUTBOX_MARKER, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => {
                assert_eq!(m.tell_id, 7);
                assert_eq!(m.crash_restart.as_deref(), Some([1, 2, 3].as_slice()));
            }
            other => panic!(
                "expected decoded TellRequested, got is_some={}",
                other.is_some()
            ),
        }

        let acked = OutboxMessage::TellAcked(TellAcked { tell_id: 9 });
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_MARKER, &acked.encode()),
            Some(Ok(OutboxMessage::TellAcked(m))) if m.tell_id == 9
        ));

        let failed = OutboxMessage::TellFailed(TellFailed { tell_id: 11 });
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_MARKER, &failed.encode()),
            Some(Ok(OutboxMessage::TellFailed(m))) if m.tell_id == 11
        ));

        let scheduled = OutboxMessage::Scheduled(Scheduled {
            at_unix_seconds: 1_700_000_000,
        });
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_MARKER, &scheduled.encode()),
            Some(Ok(OutboxMessage::Scheduled(m))) if m.at_unix_seconds == 1_700_000_000
        ));
    }

    #[test]
    fn terminal_markers_are_discriminated_by_oneof_field_not_payload_bytes() {
        let acked = OutboxMessage::TellAcked(TellAcked { tell_id: 5 });
        let failed = OutboxMessage::TellFailed(TellFailed { tell_id: 5 });

        assert_ne!(
            acked.encode(),
            failed.encode(),
            "same tell_id must not collapse acked/failed to identical bytes"
        );
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_MARKER, &acked.encode()),
            Some(Ok(OutboxMessage::TellAcked(_)))
        ));
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_MARKER, &failed.encode()),
            Some(Ok(OutboxMessage::TellFailed(_)))
        ));
    }

    #[test]
    fn tell_requested_without_crash_restart_omits_field() {
        let requested = tell_requested(1, None);
        match OutboxMessage::classify(OUTBOX_MARKER, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => {
                assert_eq!(m.tell_id, 1);
                assert!(m.crash_restart.is_none());
            }
            _ => panic!("expected decoded TellRequested with absent crash_restart"),
        }
    }

    #[test]
    fn classify_returns_none_for_user_event_types() {
        assert!(OutboxMessage::classify(
            EventType::new(Family::new("user"), TypeName::new("SomeEvent")),
            &[]
        )
        .is_none());
    }

    #[test]
    fn classify_dispatches_by_type_key_when_variant_present() {
        let requested = tell_requested(42, None);
        let incoming_with_variant = EventType::with_variant(
            OUTBOX_MARKER.family(),
            OUTBOX_MARKER.type_name(),
            Variant::new("v1"),
        );
        match OutboxMessage::classify(incoming_with_variant, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => assert_eq!(m.tell_id, 42),
            other => panic!(
                "expected TellRequested decoded via type_key, classify returned Some={}",
                other.is_some()
            ),
        }
    }

    #[test]
    fn classify_reports_decode_error_for_outbox_type_missing_oneof_kind() {
        match OutboxMessage::classify(OUTBOX_MARKER, &[]) {
            Some(Err(_)) => {}
            other => panic!(
                "expected Some(Err(_)) for outbox marker with no oneof kind, got is_some={}",
                other.is_some()
            ),
        }
    }
}
