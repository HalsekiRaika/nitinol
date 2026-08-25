//! `DeadLetterDispositionEvent` — the marker [`DeadLetterQueue`] appends when
//! an operator settles a dead letter.
//!
//! Modelled on the outbox lifecycle markers (`TellRequested → TellAcked /
//! TellFailed`): a reserved type-level [`EventType`] whose per-arm `variant()`
//! writes the disposition on the wire, with a prost `oneof` codec that owns its
//! own serialization.
//!
//! # Why a sibling family rather than a dead-letter variant
//!
//! [`DEAD_LETTER_DISPOSITION_MARKER`] is `nitinol.saga.dead_letter_disposition`
//! — a sibling of `nitinol.saga.dead_letter`, never a descendant of it.  The
//! dead-letter family's Materialized Path is what the push subscriber selects
//! and decodes on, so a marker placed *inside* that family would be handed to
//! [`DeadLetterEvent::decode`](crate::DeadLetterEvent) as if it were a failure
//! record.  Keeping the families apart leaves the existing prefix matching
//! exactly the records it matched before this marker existed.
//!
//! [`DeadLetterQueue`]: crate::DeadLetterQueue

use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{EventType, Family, TypeName, Variant};

mod proto {
    include!(concat!(
        env!("OUT_DIR"),
        "/nitinol.saga.dead_letter_disposition.rs"
    ));
}

use self::proto::dead_letter_disposition_marker::Disposition as ProtoDisposition;
use self::proto::{DeadLetterDispositionMarker, Evicted, Processed};

/// Reserved type-level identity shared by every disposition marker.
pub(crate) const DEAD_LETTER_DISPOSITION_MARKER: EventType = EventType::new(
    Family::new("nitinol.saga"),
    TypeName::new("dead_letter_disposition"),
);

#[derive(Debug, thiserror::Error)]
#[error("dead letter disposition payload carried no `oneof` disposition")]
struct MissingDisposition;

/// How an operator settled a dead letter.
///
/// Both arms remove the dead letter from
/// [`DeadLetterQueue::list`](crate::DeadLetterQueue::list); they differ in what
/// they record about *why*, which is why each writes its own wire variant
/// instead of collapsing into one "settled" marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The dead letter was handed downstream and dealt with.
    Processed,
    /// The dead letter was retired without being dealt with.
    Evicted,
}

/// A framework-managed marker recording that one dead letter on the saga's own
/// stream has been settled.
#[derive(Clone, Debug)]
pub(crate) struct DeadLetterDispositionEvent {
    /// Stream sequence of the dead letter this marker settles.  A dead letter's
    /// position on the saga stream is its identity, so this is the whole
    /// reference.
    pub(crate) dead_letter_sequence: u64,
    pub(crate) disposition: Disposition,
}

impl SystemEvent for DeadLetterDispositionEvent {
    const EVENT_TYPE: EventType = DEAD_LETTER_DISPOSITION_MARKER;

    fn variant(&self) -> EventType {
        let variant = match self.disposition {
            Disposition::Processed => Variant::new("processed"),
            Disposition::Evicted => Variant::new("evicted"),
        };
        EventType::with_variant(
            DEAD_LETTER_DISPOSITION_MARKER.family(),
            DEAD_LETTER_DISPOSITION_MARKER.type_name(),
            variant,
        )
    }

    fn encode(&self) -> Bytes {
        let disposition = match self.disposition {
            Disposition::Processed => ProtoDisposition::Processed(Processed {}),
            Disposition::Evicted => ProtoDisposition::Evicted(Evicted {}),
        };
        let marker = DeadLetterDispositionMarker {
            dead_letter_sequence: self.dead_letter_sequence,
            disposition: Some(disposition),
        };
        Bytes::from(prost::Message::encode_to_vec(&marker))
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        let marker = <DeadLetterDispositionMarker as prost::Message>::decode(payload)
            .map_err(SystemEventDecodeError::new)?;
        let disposition = match marker.disposition {
            Some(ProtoDisposition::Processed(_)) => Disposition::Processed,
            Some(ProtoDisposition::Evicted(_)) => Disposition::Evicted,
            // A marker with no disposition names a dead letter without saying
            // what became of it; defaulting either way would either resurface a
            // settled entry or hide a live one.
            None => return Err(SystemEventDecodeError::new(MissingDisposition)),
        };
        Ok(Self {
            dead_letter_sequence: marker.dead_letter_sequence,
            disposition,
        })
    }
}

/// Type-level classification: does `event_type` belong to the disposition
/// family (`nitinol.saga.dead_letter_disposition` and its per-arm variants)?
pub(crate) fn is_dead_letter_disposition_event_type(event_type: EventType) -> bool {
    event_type
        .to_path()
        .is_within(&DEAD_LETTER_DISPOSITION_MARKER.to_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dead_letter::event::DEAD_LETTER_MARKER;

    #[test]
    fn disposition_reserves_a_single_type_level_event_type() {
        assert_eq!(
            DeadLetterDispositionEvent::EVENT_TYPE,
            DEAD_LETTER_DISPOSITION_MARKER
        );
        assert_eq!(
            DEAD_LETTER_DISPOSITION_MARKER.to_string(),
            "nitinol.saga.dead_letter_disposition"
        );
        assert!(
            DEAD_LETTER_DISPOSITION_MARKER.variant().is_none(),
            "the reserved disposition type is type-level (variant None)"
        );
    }

    #[test]
    fn each_disposition_writes_its_per_arm_variant_on_the_wire() {
        for (disposition, expected) in [
            (Disposition::Processed, "processed"),
            (Disposition::Evicted, "evicted"),
        ] {
            let marker = DeadLetterDispositionEvent {
                dead_letter_sequence: 3,
                disposition,
            };
            assert_eq!(marker.variant().variant(), Some(Variant::new(expected)));
            assert_eq!(
                marker.variant().type_key(),
                DEAD_LETTER_DISPOSITION_MARKER.type_key()
            );
        }
    }

    #[test]
    fn each_disposition_round_trips_through_the_enum_codec() {
        for disposition in [Disposition::Processed, Disposition::Evicted] {
            let marker = DeadLetterDispositionEvent {
                dead_letter_sequence: 42,
                disposition,
            };
            match DeadLetterDispositionEvent::decode(&marker.encode()) {
                Ok(decoded) => {
                    assert_eq!(decoded.dead_letter_sequence, 42);
                    assert_eq!(
                        decoded.disposition, disposition,
                        "a decoded marker must keep the arm it was written as; \
                         collapsing the two would erase why the dead letter was settled"
                    );
                }
                Err(e) => panic!("decode failed: {e}"),
            }
        }
    }

    /// The two arms must not collapse to the same bytes for the same target —
    /// otherwise `processed` and `evicted` would be indistinguishable on the
    /// wire wherever the event type is not consulted.
    #[test]
    fn processed_and_evicted_are_discriminated_by_the_oneof_field() {
        let processed = DeadLetterDispositionEvent {
            dead_letter_sequence: 5,
            disposition: Disposition::Processed,
        };
        let evicted = DeadLetterDispositionEvent {
            dead_letter_sequence: 5,
            disposition: Disposition::Evicted,
        };
        assert_ne!(processed.encode(), evicted.encode());
    }

    #[test]
    fn decode_reports_error_for_payload_missing_oneof_disposition() {
        assert!(DeadLetterDispositionEvent::decode(&[]).is_err());
    }

    /// The classification that keeps the push path blind to these markers: the
    /// disposition family is a sibling of the dead-letter family, so neither
    /// prefix selects the other's records.
    #[test]
    fn the_disposition_family_and_the_dead_letter_family_are_disjoint_siblings() {
        let processed = EventType::with_variant(
            DEAD_LETTER_DISPOSITION_MARKER.family(),
            DEAD_LETTER_DISPOSITION_MARKER.type_name(),
            Variant::new("processed"),
        );

        assert!(is_dead_letter_disposition_event_type(
            DEAD_LETTER_DISPOSITION_MARKER
        ));
        assert!(is_dead_letter_disposition_event_type(processed));

        assert!(
            !processed.to_path().is_within(&DEAD_LETTER_MARKER.to_path()),
            "a disposition marker must not fall under the dead-letter prefix — \
             that prefix is what the push subscriber decodes on"
        );
        assert!(
            !is_dead_letter_disposition_event_type(DEAD_LETTER_MARKER),
            "a dead letter must not be mistaken for its own disposition marker"
        );
        assert!(!is_dead_letter_disposition_event_type(EventType::new(
            Family::new("nitinol.saga"),
            TypeName::new("outbox"),
        )));
    }
}
