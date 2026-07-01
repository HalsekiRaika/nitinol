//! Outbox marker messages and their closed-enum classifier.
//!
//! The prost-generated structs ([`TellRequested`], [`TellAcked`],
//! [`TellFailed`], [`Scheduled`]) each implement [`SystemEvent`] so they carry
//! their own wire codec and reserved [`EventType`].  [`OutboxMessage`] is the
//! closed enum the replay path matches on: adding a marker variant forces every
//! `match` over `OutboxMessage` to be updated, turning a missed marker into a
//! compile error instead of a silent runtime mis-classification.

use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::EventType;

use crate::outbox::event_types::{
    OUTBOX_SCHEDULED, OUTBOX_TELL_ACKED, OUTBOX_TELL_FAILED, OUTBOX_TELL_REQUESTED,
};

mod proto {
    include!(concat!(env!("OUT_DIR"), "/nitinol.saga.outbox.rs"));
}

pub(crate) use self::proto::{Scheduled, TellAcked, TellFailed, TellRequested};

macro_rules! impl_system_event {
    ($ty:ty, $event_type:expr) => {
        impl SystemEvent for $ty {
            const EVENT_TYPE: EventType = $event_type;

            fn encode(&self) -> Bytes {
                Bytes::from(prost::Message::encode_to_vec(self))
            }

            fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
                <Self as prost::Message>::decode(payload).map_err(SystemEventDecodeError::new)
            }
        }
    };
}

impl_system_event!(TellRequested, OUTBOX_TELL_REQUESTED);
impl_system_event!(TellAcked, OUTBOX_TELL_ACKED);
impl_system_event!(TellFailed, OUTBOX_TELL_FAILED);
impl_system_event!(Scheduled, OUTBOX_SCHEDULED);

/// The closed set of framework outbox markers.
///
/// Every variant is a decoded [`SystemEvent`]; the replay path matches over
/// this enum so the marker taxonomy is exhaustively checked by the compiler.
pub(crate) enum OutboxMessage {
    TellRequested(TellRequested),
    TellAcked(TellAcked),
    TellFailed(TellFailed),
    Scheduled(Scheduled),
}

impl OutboxMessage {
    /// Classify a loaded event by its [`EventType`].
    ///
    /// Returns `None` for non-outbox (user) events, `Some(Ok(_))` for a decoded
    /// marker, and `Some(Err(_))` when the event is an outbox marker but its
    /// payload fails to decode.
    pub(crate) fn classify(
        event_type: EventType,
        payload: &[u8],
    ) -> Option<Result<OutboxMessage, SystemEventDecodeError>> {
        if event_type.type_key() == OUTBOX_TELL_REQUESTED.type_key() {
            Some(TellRequested::decode(payload).map(OutboxMessage::TellRequested))
        } else if event_type.type_key() == OUTBOX_TELL_ACKED.type_key() {
            Some(TellAcked::decode(payload).map(OutboxMessage::TellAcked))
        } else if event_type.type_key() == OUTBOX_TELL_FAILED.type_key() {
            Some(TellFailed::decode(payload).map(OutboxMessage::TellFailed))
        } else if event_type.type_key() == OUTBOX_SCHEDULED.type_key() {
            Some(Scheduled::decode(payload).map(OutboxMessage::Scheduled))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitinol_persistence::{Family, TypeName, Variant};

    #[test]
    fn classify_round_trips_each_marker_through_its_system_event_codec() {
        let requested = TellRequested {
            tell_id: 7,
            crash_restart: Some(vec![1, 2, 3]),
        };
        match OutboxMessage::classify(OUTBOX_TELL_REQUESTED, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => {
                assert_eq!(m.tell_id, 7);
                assert_eq!(m.crash_restart.as_deref(), Some([1, 2, 3].as_slice()));
            }
            other => panic!("expected decoded TellRequested, got {:?}", other.is_some()),
        }

        let acked = TellAcked { tell_id: 9 };
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_TELL_ACKED, &acked.encode()),
            Some(Ok(OutboxMessage::TellAcked(m))) if m.tell_id == 9
        ));

        let failed = TellFailed { tell_id: 11 };
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_TELL_FAILED, &failed.encode()),
            Some(Ok(OutboxMessage::TellFailed(m))) if m.tell_id == 11
        ));

        let scheduled = Scheduled {
            at_unix_seconds: 1_700_000_000,
        };
        assert!(matches!(
            OutboxMessage::classify(OUTBOX_SCHEDULED, &scheduled.encode()),
            Some(Ok(OutboxMessage::Scheduled(m))) if m.at_unix_seconds == 1_700_000_000
        ));
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
    fn tell_requested_without_crash_restart_omits_field() {
        let requested = TellRequested {
            tell_id: 1,
            crash_restart: None,
        };
        match OutboxMessage::classify(OUTBOX_TELL_REQUESTED, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => {
                assert_eq!(m.tell_id, 1);
                assert!(m.crash_restart.is_none());
            }
            _ => panic!("expected decoded TellRequested with absent crash_restart"),
        }
    }

    /// Regression: classify uses type_key() so a variant-Some EventType with the
    /// same family/type_name as an outbox marker must still dispatch and decode.
    ///
    /// If classify reverted to full `==` instead of `.type_key() ==`, this test
    /// would return `None` instead of `Some(Ok(TellRequested(...)))` and fail.
    #[test]
    fn classify_with_variant_some_dispatches_by_type_key() {
        let requested = TellRequested {
            tell_id: 42,
            crash_restart: None,
        };
        // Same family/type_name as OUTBOX_TELL_REQUESTED, but carrying a variant.
        let incoming_with_variant = EventType::with_variant(
            Family::new("nitinol.saga.outbox"),
            TypeName::new("tell_requested"),
            Variant::new("v1"),
        );
        match OutboxMessage::classify(incoming_with_variant, &requested.encode()) {
            Some(Ok(OutboxMessage::TellRequested(m))) => {
                assert_eq!(m.tell_id, 42);
            }
            other => panic!(
                "expected TellRequested decoded via type_key, classify returned Some={}",
                other.is_some()
            ),
        }
    }
}
