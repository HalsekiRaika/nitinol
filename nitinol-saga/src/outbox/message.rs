use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{EventType, Family, TypeName, Variant};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.outbox.rs"));
}

pub(crate) use self::proto::{
    Ended, OutboxMarker, Scheduled, TellAcked, TellFailed, TellRequested,
};

use self::proto::outbox_marker::Kind;

pub(crate) const OUTBOX_MARKER: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("outbox"));

#[derive(Debug, thiserror::Error)]
#[error("outbox marker payload carried no `oneof` kind")]
struct MissingOutboxKind;

pub(crate) enum OutboxEvent {
    TellRequested(TellRequested),
    TellAcked(TellAcked),
    TellFailed(TellFailed),
    Scheduled(Scheduled),
    Ended(Ended),
}

impl SystemEvent for OutboxEvent {
    const EVENT_TYPE: EventType = OUTBOX_MARKER;

    fn variant(&self) -> EventType {
        let variant = match self {
            OutboxEvent::TellRequested(_) => Variant::new("tell_requested"),
            OutboxEvent::TellAcked(_) => Variant::new("tell_acked"),
            OutboxEvent::TellFailed(_) => Variant::new("tell_failed"),
            OutboxEvent::Scheduled(_) => Variant::new("scheduled"),
            OutboxEvent::Ended(_) => Variant::new("ended"),
        };
        EventType::with_variant(OUTBOX_MARKER.family(), OUTBOX_MARKER.type_name(), variant)
    }

    fn encode(&self) -> Bytes {
        let kind = match self {
            OutboxEvent::TellRequested(m) => Kind::TellRequested(m.clone()),
            OutboxEvent::TellAcked(m) => Kind::TellAcked(*m),
            OutboxEvent::TellFailed(m) => Kind::TellFailed(*m),
            OutboxEvent::Scheduled(m) => Kind::Scheduled(*m),
            OutboxEvent::Ended(m) => Kind::Ended(*m),
        };
        let marker = OutboxMarker { kind: Some(kind) };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <OutboxMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        match marker.kind {
            Some(Kind::TellRequested(m)) => Ok(OutboxEvent::TellRequested(m)),
            Some(Kind::TellAcked(m)) => Ok(OutboxEvent::TellAcked(m)),
            Some(Kind::TellFailed(m)) => Ok(OutboxEvent::TellFailed(m)),
            Some(Kind::Scheduled(m)) => Ok(OutboxEvent::Scheduled(m)),
            Some(Kind::Ended(m)) => Ok(OutboxEvent::Ended(m)),
            None => Err(SystemEventDecodeError::new(MissingOutboxKind)),
        }
    }
}

pub(crate) fn is_outbox_event_type(event_type: EventType) -> bool {
    event_type.to_path().is_within(&OUTBOX_MARKER.to_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitinol_persistence::{Family, TypeName, Variant};

    fn tell_requested(tell_id: u64, crash_restart: Option<Vec<u8>>) -> OutboxEvent {
        OutboxEvent::TellRequested(TellRequested {
            tell_id,
            crash_restart,
            target: String::new(),
        })
    }

    #[test]
    fn outbox_event_reserves_a_single_event_type() {
        assert_eq!(OutboxEvent::EVENT_TYPE, OUTBOX_MARKER);
        assert_eq!(OUTBOX_MARKER.to_string(), "nitinol.saga.outbox");
        assert!(
            OUTBOX_MARKER.variant().is_none(),
            "the reserved outbox type is type-level (variant None)"
        );
    }

    #[test]
    fn each_marker_writes_its_per_arm_variant_on_the_wire() {
        let cases = [
            (tell_requested(1, None), "tell_requested"),
            (
                OutboxEvent::TellAcked(TellAcked { tell_id: 1 }),
                "tell_acked",
            ),
            (
                OutboxEvent::TellFailed(TellFailed { tell_id: 1 }),
                "tell_failed",
            ),
            (
                OutboxEvent::Scheduled(Scheduled { at_unix_seconds: 0 }),
                "scheduled",
            ),
        ];
        for (marker, expected) in &cases {
            assert_eq!(marker.variant().variant(), Some(Variant::new(expected)));
            assert_eq!(marker.variant().type_key(), OUTBOX_MARKER.type_key());
        }
    }

    #[test]
    fn each_marker_round_trips_through_the_enum_codec() {
        let requested = tell_requested(7, Some(vec![1, 2, 3]));
        match OutboxEvent::decode(&requested.encode()) {
            Ok(OutboxEvent::TellRequested(m)) => {
                assert_eq!(m.tell_id, 7);
                assert_eq!(m.crash_restart.as_deref(), Some([1, 2, 3].as_slice()));
            }
            _ => panic!("expected decoded TellRequested"),
        }

        let acked = OutboxEvent::TellAcked(TellAcked { tell_id: 9 });
        assert!(matches!(
            OutboxEvent::decode(&acked.encode()),
            Ok(OutboxEvent::TellAcked(m)) if m.tell_id == 9
        ));

        let failed = OutboxEvent::TellFailed(TellFailed { tell_id: 11 });
        assert!(matches!(
            OutboxEvent::decode(&failed.encode()),
            Ok(OutboxEvent::TellFailed(m)) if m.tell_id == 11
        ));

        let scheduled = OutboxEvent::Scheduled(Scheduled {
            at_unix_seconds: 1_700_000_000,
        });
        assert!(matches!(
            OutboxEvent::decode(&scheduled.encode()),
            Ok(OutboxEvent::Scheduled(m)) if m.at_unix_seconds == 1_700_000_000
        ));
    }

    #[test]
    fn terminal_markers_are_discriminated_by_oneof_field_not_payload_bytes() {
        let acked = OutboxEvent::TellAcked(TellAcked { tell_id: 5 });
        let failed = OutboxEvent::TellFailed(TellFailed { tell_id: 5 });

        assert_ne!(
            acked.encode(),
            failed.encode(),
            "same tell_id must not collapse acked/failed to identical bytes"
        );
        assert!(matches!(
            OutboxEvent::decode(&acked.encode()),
            Ok(OutboxEvent::TellAcked(_))
        ));
        assert!(matches!(
            OutboxEvent::decode(&failed.encode()),
            Ok(OutboxEvent::TellFailed(_))
        ));
    }

    #[test]
    fn tell_requested_without_crash_restart_omits_field() {
        let requested = tell_requested(1, None);
        match OutboxEvent::decode(&requested.encode()) {
            Ok(OutboxEvent::TellRequested(m)) => {
                assert_eq!(m.tell_id, 1);
                assert!(m.crash_restart.is_none());
            }
            _ => panic!("expected decoded TellRequested with absent crash_restart"),
        }
    }

    #[test]
    fn is_outbox_event_type_rejects_user_event_types() {
        assert!(!is_outbox_event_type(EventType::new(
            Family::new("user"),
            TypeName::new("SomeEvent")
        )));
    }

    #[test]
    fn is_outbox_event_type_accepts_reserved_type_and_its_per_arm_variants() {
        assert!(is_outbox_event_type(OUTBOX_MARKER));
        let with_variant = EventType::with_variant(
            OUTBOX_MARKER.family(),
            OUTBOX_MARKER.type_name(),
            Variant::new("tell_requested"),
        );
        assert!(is_outbox_event_type(with_variant));
    }

    #[test]
    fn decode_reports_error_for_outbox_payload_missing_oneof_kind() {
        assert!(OutboxEvent::decode(&[]).is_err());
    }
}
