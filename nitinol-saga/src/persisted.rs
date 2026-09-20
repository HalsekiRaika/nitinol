use bytes::Bytes;
use nitinol_eventsource::codec;
use nitinol_eventsource::error::CodecError;
use nitinol_eventsource::{Event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::EventType;

use crate::dead_letter::{DeadLetterDispositionEvent, DeadLetterEvent};
use crate::outbox::OutboxEvent;
use crate::scheduler::ScheduleEvent;

pub(crate) enum SagaPersisted<E> {
    Domain(E),
    Outbox(OutboxEvent),
    Schedule(ScheduleEvent),
    DeadLetter(DeadLetterEvent),
    DeadLetterDisposition(DeadLetterDispositionEvent),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SagaPersistedDecodeError {
    #[error("outbox marker decode failed: {0}")]
    Outbox(#[from] SystemEventDecodeError),
    #[error("schedule marker decode failed: {0}")]
    Schedule(SystemEventDecodeError),
    #[error("dead letter decode failed: {0}")]
    DeadLetter(SystemEventDecodeError),
    #[error("dead letter disposition marker decode failed: {0}")]
    DeadLetterDisposition(SystemEventDecodeError),
    #[error("domain event decode failed: {0}")]
    Domain(#[from] CodecError),
}

impl<E: Event> SagaPersisted<E> {
    /// The canonical path a row of this envelope's content carries.
    pub(crate) fn event_type(&self) -> EventType {
        match self {
            SagaPersisted::Domain(_) => E::EVENT_TYPE,
            SagaPersisted::Outbox(_) => OutboxEvent::EVENT_TYPE,
            SagaPersisted::Schedule(_) => ScheduleEvent::EVENT_TYPE,
            SagaPersisted::DeadLetter(_) => DeadLetterEvent::EVENT_TYPE,
            SagaPersisted::DeadLetterDisposition(_) => DeadLetterDispositionEvent::EVENT_TYPE,
        }
    }

    pub(crate) fn encode(&self) -> Result<Bytes, CodecError> {
        match self {
            SagaPersisted::Domain(event) => codec::encode::<E::Codec, E>(event),
            SagaPersisted::Outbox(marker) => Ok(marker.encode()),
            SagaPersisted::Schedule(marker) => Ok(marker.encode()),
            SagaPersisted::DeadLetter(event) => Ok(event.encode()),
            SagaPersisted::DeadLetterDisposition(marker) => Ok(marker.encode()),
        }
    }

    /// Read a stored row back as the content its declaration names.
    ///
    /// The four framework declarations
    /// (`nitinol.saga.{outbox,schedule,dead_letter,dead_letter_disposition}`)
    /// are siblings, so a row matches at most one of them and the order between
    /// those arms carries no meaning.  Every other path belongs to the domain
    /// and is handed to the user's own codec untouched — that catch-all is what
    /// keeps the envelope transparent to event types this crate never declared.
    pub(crate) fn classify(
        event_type: EventType,
        payload: &[u8],
    ) -> Result<Self, SagaPersistedDecodeError> {
        match event_type {
            OutboxEvent::EVENT_TYPE => Ok(SagaPersisted::Outbox(OutboxEvent::decode(payload)?)),
            ScheduleEvent::EVENT_TYPE => ScheduleEvent::decode(payload)
                .map(SagaPersisted::Schedule)
                .map_err(SagaPersistedDecodeError::Schedule),
            DeadLetterEvent::EVENT_TYPE => DeadLetterEvent::decode(payload)
                .map(SagaPersisted::DeadLetter)
                .map_err(SagaPersistedDecodeError::DeadLetter),
            DeadLetterDispositionEvent::EVENT_TYPE => DeadLetterDispositionEvent::decode(payload)
                .map(SagaPersisted::DeadLetterDisposition)
                .map_err(SagaPersistedDecodeError::DeadLetterDisposition),
            _ => Ok(SagaPersisted::Domain(codec::decode::<E::Codec, E>(
                payload,
            )?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitinol_eventsource::codec::Codec;
    use nitinol_persistence::{Family, TypeName};
    use serde::{Deserialize, Serialize};

    use crate::dead_letter::{Disposition, SagaFailure, SourceContext};
    use crate::id::SagaId;
    use crate::outbox::OutboxAppender;
    use crate::scheduler::{ScheduleToken, TimerName};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct DomainEvt {
        note: String,
    }

    struct JsonCodec;

    impl<E: Serialize + for<'de> Deserialize<'de> + 'static> Codec<E> for JsonCodec {
        type Error = serde_json::Error;

        fn encode(event: &E) -> Result<Bytes, Self::Error> {
            serde_json::to_vec(event).map(Bytes::from)
        }

        fn decode(payload: &[u8]) -> Result<E, Self::Error> {
            serde_json::from_slice(payload)
        }
    }

    impl Event for DomainEvt {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("persisted_unit_test"), TypeName::new("Domain"));

        type Codec = JsonCodec;
    }

    #[test]
    fn non_outbox_path_classifies_as_domain_and_round_trips() {
        let event = DomainEvt {
            note: "hello".to_owned(),
        };
        let payload = <JsonCodec as Codec<DomainEvt>>::encode(&event).expect("encode");

        match SagaPersisted::<DomainEvt>::classify(DomainEvt::EVENT_TYPE, &payload) {
            Ok(SagaPersisted::Domain(decoded)) => assert_eq!(decoded, event),
            _ => panic!("a non-outbox path must classify as Domain"),
        }
    }

    #[test]
    fn outbox_path_classifies_as_outbox_never_domain() {
        let wire =
            OutboxAppender::build_tell_requested(1, 7, None, "", jiff::Timestamp::UNIX_EPOCH);

        match SagaPersisted::<DomainEvt>::classify(wire.event_type, &wire.payload) {
            Ok(SagaPersisted::Outbox(OutboxEvent::TellRequested(m))) => assert_eq!(m.tell_id, 7),
            _ => panic!("an outbox path must classify as Outbox"),
        }
    }

    #[test]
    fn schedule_path_classifies_as_schedule_never_domain() {
        let marker = ScheduleEvent::Fired {
            token: ScheduleToken {
                saga_id: SagaId::new("saga-1"),
                name: TimerName::new("reminder"),
            },
            fired_at_unix_millis: 1_700_000_000_000,
        };

        match SagaPersisted::<DomainEvt>::classify(ScheduleEvent::EVENT_TYPE, &marker.encode()) {
            Ok(SagaPersisted::Schedule(ScheduleEvent::Fired { token, .. })) => {
                assert_eq!(token.name, TimerName::new("reminder"));
            }
            _ => panic!("a schedule path must classify as Schedule"),
        }
    }

    #[test]
    fn dead_letter_path_classifies_as_dead_letter_never_domain() {
        let record = DeadLetterEvent {
            seq: 3,
            saga_id: SagaId::new("saga-1"),
            failure: SagaFailure::HandleFailed {
                error: "boom".to_owned(),
            },
            occurred_at_unix_millis: 1_700_000_000_000,
            source: SourceContext::without_upstream(),
        };

        match SagaPersisted::<DomainEvt>::classify(DeadLetterEvent::EVENT_TYPE, &record.encode()) {
            Ok(SagaPersisted::DeadLetter(decoded)) => assert_eq!(decoded.seq, 3),
            _ => panic!("a dead-letter path must classify as DeadLetter"),
        }
    }

    #[test]
    fn dead_letter_disposition_path_classifies_as_disposition_never_dead_letter() {
        let marker = DeadLetterDispositionEvent {
            dead_letter_sequence: 3,
            disposition: Disposition::Evicted,
        };

        match SagaPersisted::<DomainEvt>::classify(
            DeadLetterDispositionEvent::EVENT_TYPE,
            &marker.encode(),
        ) {
            Ok(SagaPersisted::DeadLetterDisposition(decoded)) => {
                assert_eq!(decoded.dead_letter_sequence, 3);
            }
            _ => panic!("a disposition path must classify as DeadLetterDisposition"),
        }
    }

    #[test]
    fn encode_is_transparent_for_domain_events() {
        let event = DomainEvt {
            note: "raw".to_owned(),
        };
        let via_envelope = SagaPersisted::Domain(event.clone())
            .encode()
            .expect("envelope encode");
        let direct = <JsonCodec as Codec<DomainEvt>>::encode(&event).expect("direct encode");

        assert_eq!(
            via_envelope, direct,
            "the envelope must return the inner codec bytes verbatim (no double-encode)"
        );
        assert_eq!(
            SagaPersisted::Domain(event).event_type(),
            DomainEvt::EVENT_TYPE,
            "the envelope must expose the inner event's own wire identity"
        );
    }

    #[test]
    fn undecodable_domain_payload_surfaces_a_domain_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(DomainEvt::EVENT_TYPE, b"not json") {
            Err(SagaPersistedDecodeError::Domain(_)) => {}
            _ => panic!("a bad domain payload must surface a Domain decode error"),
        }
    }

    #[test]
    fn undecodable_outbox_payload_surfaces_an_outbox_decode_error() {
        let outbox_type =
            OutboxAppender::build_tell_requested(1, 1, None, "", jiff::Timestamp::UNIX_EPOCH)
                .event_type;
        match SagaPersisted::<DomainEvt>::classify(outbox_type, &[]) {
            Err(SagaPersistedDecodeError::Outbox(_)) => {}
            _ => panic!("an outbox marker with no oneof kind must surface an Outbox decode error"),
        }
    }

    #[test]
    fn undecodable_dead_letter_disposition_payload_surfaces_a_disposition_decode_error() {
        match SagaPersisted::<DomainEvt>::classify(DeadLetterDispositionEvent::EVENT_TYPE, &[]) {
            Err(SagaPersistedDecodeError::DeadLetterDisposition(_)) => {}
            _ => panic!(
                "a disposition marker with no oneof kind must surface a DeadLetterDisposition decode error"
            ),
        }
    }
}
