//! `DeadLetterDispositionEvent` — the marker [`DeadLetterQueue`] appends when
//! an operator settles a dead letter.
//!
//! Modelled on the outbox lifecycle markers (`TellRequested → TellAcked /
//! TellFailed`): one reserved [`EventType`] every disposition marker is written
//! down as, with a prost `oneof` codec that owns its own serialization and
//! carries which settlement the marker records.
//!
//! # Why a sibling declaration rather than a dead-letter arm
//!
//! [`DEAD_LETTER_DISPOSITION_MARKER`] is `nitinol.saga.dead_letter_disposition`
//! — a sibling of `nitinol.saga.dead_letter`, never the same declaration.  A
//! dead letter is selected and decoded by the path it carries, so a settlement
//! written under the dead letter's own path would be handed to
//! [`DeadLetterEvent::decode`](crate::DeadLetterEvent) as if it were a failure
//! record.  Keeping the two declarations apart leaves the push subscriber
//! matching exactly the records it matched before this marker existed.
//!
//! [`DeadLetterQueue`]: crate::DeadLetterQueue

use bytes::Bytes;
use nitinol_eventsource::{SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{reserved_event, EventType};

mod proto {
    include!(concat!(
        env!("OUT_DIR"),
        "/nitinol.saga.dead_letter_disposition.rs"
    ));
}

use self::proto::dead_letter_disposition_marker::Disposition as ProtoDisposition;
use self::proto::{DeadLetterDispositionMarker, Evicted, Processed};

reserved_event! {
    /// The one identity every disposition marker is written down as.
    ///
    /// What a binary linking this crate collects for the disposition marker, so
    /// that a load of a saga's own stream restores a settled dead letter rather
    /// than failing on it.
    pub(crate) const DEAD_LETTER_DISPOSITION_MARKER {
        family = "nitinol.saga",
        type_name = "dead_letter_disposition",
        declared_by = DeadLetterDispositionEvent,
    }

    // The settlements a disposition marker records — the marker's whole arm
    // set. A stored row names the declaration, so the settlement lives in the
    // payload's `oneof`; these are the names that `oneof` is reported under.
    // The settlement lives on the `disposition` field, so each pattern looks
    // into it rather than matching the marker itself.
    arm_type = DeadLetterDispositionEvent;
    DeadLetterDispositionEvent { disposition: Disposition::Processed, .. } => PROCESSED_ARM = "processed",
    DeadLetterDispositionEvent { disposition: Disposition::Evicted, .. } => EVICTED_ARM = "evicted",
}

#[derive(Debug, thiserror::Error)]
#[error("dead letter disposition payload carried no `oneof` disposition")]
struct MissingDisposition;

/// How an operator settled a dead letter.
///
/// Both arms remove the dead letter from
/// [`DeadLetterQueue::list`](crate::DeadLetterQueue::list); they differ in what
/// they record about *why*, which is why each writes its own `oneof` arm
/// instead of collapsing into one "settled" payload.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_reserves_a_single_event_type() {
        assert_eq!(
            DeadLetterDispositionEvent::EVENT_TYPE,
            DEAD_LETTER_DISPOSITION_MARKER
        );
        assert_eq!(
            DEAD_LETTER_DISPOSITION_MARKER.to_string(),
            "nitinol.saga.dead_letter_disposition"
        );
    }

    /// The arm a marker reports is the name of the `oneof` its payload carries.
    /// The column no longer distinguishes them, so this is the whole of what an
    /// operator has to tell one settlement from another.
    #[test]
    fn each_disposition_reports_the_arm_its_payload_carries() {
        for (disposition, expected) in [
            (Disposition::Processed, "processed"),
            (Disposition::Evicted, "evicted"),
        ] {
            let marker = DeadLetterDispositionEvent {
                dead_letter_sequence: 3,
                disposition,
            };
            assert_eq!(marker.arm(), expected);
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
}
