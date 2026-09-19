use bytes::Bytes;
use linkme::distributed_slice;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::__private::{reserved_event_registry_entry, reserved_family};
use nitinol_persistence::{EventRegistryEntry, EventType, TypeName, EVENT_REGISTRY};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.outbox.rs"));
}

pub(crate) use self::proto::{
    Ended, OutboxMarker, Scheduled, TellAcked, TellFailed, TellRequested,
};

use self::proto::outbox_marker::Kind;

/// The one identity every outbox marker is written down as.
pub(crate) const OUTBOX_MARKER: EventType =
    EventType::new(reserved_family("nitinol.saga"), TypeName::new("outbox"));

/// The transitions an outbox marker records — the outbox's whole arm set.
///
/// A stored row names the declaration, so the transition lives in the payload's
/// `oneof`; these are the names that `oneof` is reported under.
pub(crate) const TELL_REQUESTED_ARM: &str = "tell_requested";
const TELL_ACKED_ARM: &str = "tell_acked";
const TELL_FAILED_ARM: &str = "tell_failed";
const SCHEDULED_ARM: &str = "scheduled";
const ENDED_ARM: &str = "ended";

/// What a binary linking this crate collects for the outbox, so that a load of
/// a saga's own stream restores its markers rather than failing on them.
#[distributed_slice(EVENT_REGISTRY)]
static OUTBOX_ENTRY: EventRegistryEntry =
    reserved_event_registry_entry(concat!(module_path!(), "::OutboxEvent"), &[OUTBOX_MARKER]);

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

impl OutboxEvent {
    /// The transition this marker records, as the name its payload's `oneof`
    /// arm is reported under.
    pub(crate) const fn arm(&self) -> &'static str {
        match self {
            OutboxEvent::TellRequested(_) => TELL_REQUESTED_ARM,
            OutboxEvent::TellAcked(_) => TELL_ACKED_ARM,
            OutboxEvent::TellFailed(_) => TELL_FAILED_ARM,
            OutboxEvent::Scheduled(_) => SCHEDULED_ARM,
            OutboxEvent::Ended(_) => ENDED_ARM,
        }
    }
}

impl SystemEvent for OutboxEvent {
    const EVENT_TYPE: EventType = OUTBOX_MARKER;

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

/// Is `event_type` the outbox marker's own declaration?
///
/// A declaration answers for its own path and for no path beneath it, so this
/// is an identity test rather than a prefix one.
pub(crate) fn is_outbox_event_type(event_type: EventType) -> bool {
    event_type == OUTBOX_MARKER
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitinol_persistence::{Family, TypeName};

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
    }

    /// The arm a marker reports is the name of the `oneof` its payload carries.
    /// The column no longer distinguishes them, so this is the whole of what an
    /// operator has to tell one outbox marker from another.
    #[test]
    fn each_marker_reports_the_transition_its_payload_carries() {
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
            (OutboxEvent::Ended(Ended {}), "ended"),
        ];
        for (marker, expected) in &cases {
            assert_eq!(marker.arm(), *expected);
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
    fn is_outbox_event_type_accepts_the_declaration_and_nothing_beneath_it() {
        assert!(is_outbox_event_type(OUTBOX_MARKER));
        assert!(
            !is_outbox_event_type(EventType::new(
                nitinol_persistence::__private::reserved_family("nitinol.saga.outbox"),
                TypeName::new("tell_requested"),
            )),
            "no row carries a path beneath the outbox declaration, so nothing may be classified \
             as one"
        );
    }

    #[test]
    fn decode_reports_error_for_outbox_payload_missing_oneof_kind() {
        assert!(OutboxEvent::decode(&[]).is_err());
    }
}
